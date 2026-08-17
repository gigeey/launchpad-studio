//! Unit tests for the Bash tool.

use ao_engine_tools_core::{
    IoTool, PermissionContext, PermissionDecision, PermissionMode, Registry, RunnerContext,
    ToolOutput,
};
use std::path::PathBuf;

use super::{execute, prompt, BashTool};

const DEFAULT_TIMEOUT: u64 = 120_000;

fn test_ctx() -> RunnerContext {
    RunnerContext::new("sess", "agent").unwrap()
}

// --- scaffold ---

#[test]
fn description_drift_guard() {
    assert_eq!(BashTool::default().description(), prompt::DESCRIPTION);
}

#[test]
fn register_bash_lookup_succeeds() {
    let mut registry = Registry::new();
    super::register_bash(&mut registry);
    assert!(registry.lookup_io("Bash").is_some());
}

// --- foreground spawn ---

#[cfg(unix)]
#[tokio::test]
async fn bash_echo_hello() {
    let ctx = test_ctx();
    let outcome = execute::run("echo hello", &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&outcome.stdout).contains("hello"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_stderr_redirect() {
    let ctx = test_ctx();
    let outcome = execute::run("echo err 1>&2", &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&outcome.stderr).contains("err"));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_nonzero_exit() {
    let ctx = test_ctx();
    let outcome = execute::run("false", &ctx, DEFAULT_TIMEOUT).await.unwrap();
    assert_eq!(outcome.exit_status, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn bash_invalid_command() {
    let ctx = test_ctx();
    let outcome = execute::run("commanddoesnotexist123", &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert_ne!(outcome.exit_status, 0);
    assert!(!outcome.stderr.is_empty());
}

// --- SIGTERM-ignoring child, shared by the timeout and cancellation tests ---

/// Timeout for the tests that must let the child install its SIGTERM trap
/// before termination begins.
///
/// `trap` is a shell builtin that runs *after* fork and exec, so there is a
/// window at startup where the child still has the default SIGTERM
/// disposition. A signal delivered inside that window kills it outright and
/// the escalation path under test never runs. Measured worst case for reaching
/// the trap was 192 ms with the machine at 2x oversubscription; this leaves an
/// order of magnitude above that. The cancellation variant does not need the
/// margin — it waits for the handshake below — but the timeout variant has no
/// way to synchronise against a deadline that `run` starts internally.
#[cfg(unix)]
const TRAP_SETUP_TIMEOUT_MS: u64 = 2_000;

/// Command that makes SIGTERM ineffective and then blocks, touching `ready`
/// once the trap is actually in place.
///
/// `trap ''` sets `SIG_IGN` rather than installing a handler, and an ignored
/// disposition is inherited across both fork and exec — so the `sleep` is immune
/// to SIGTERM too. That is what forces the grace period to expire and SIGKILL to
/// be the reaper.
#[cfg(unix)]
fn term_ignoring_command(ready: &std::path::Path) -> String {
    format!("trap '' TERM; touch '{}'; sleep 30", ready.display())
}

/// Block until the child reports its SIGTERM trap is installed, so termination
/// cannot be raced against shell startup. Polling a file the child touches is
/// what makes this independent of machine load: an arbitrarily slow child is
/// waited for rather than assumed ready after a fixed sleep.
#[cfg(unix)]
async fn await_trap_installed(ready: &std::path::Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "child never signalled that its SIGTERM trap was installed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

// --- timeout + middle-truncation ---

/// SIGTERM path: `sleep 10` with a 200 ms timeout.
///
/// `timed_out` alone is only a flag — it is set by the code initiating
/// termination, not by anything observing the child die. `signal` is the
/// observation: it carries the signal the kernel actually reaped the child
/// with. A `run` that set the flag and then waited out the full `sleep 10`
/// would report `signal: None` and `exit_status: 0`, because an undisturbed
/// `sleep` exits normally.
#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_term() {
    let ctx = test_ctx();
    let outcome = execute::run("sleep 10", &ctx, 200).await.unwrap();
    assert!(outcome.timed_out, "expected timed_out = true");
    assert_eq!(
        outcome.signal,
        Some(libc::SIGTERM),
        "timeout must reap the child with SIGTERM; signal = {:?}, exit_status = {} \
         (None/0 means the child exited on its own and was never reaped)",
        outcome.signal,
        outcome.exit_status
    );
}

/// SIGKILL fallback: `trap '' TERM` sets SIGTERM to `SIG_IGN`, and an ignored
/// disposition is inherited across both fork and exec — so the `sleep` is immune
/// to SIGTERM too, the grace period expires, and SIGKILL is what ends it.
///
/// Two independent claims, neither of which the wall clock could distinguish:
/// `signal` proves SIGKILL — not SIGTERM — was the reaper, and the elapsed
/// **lower** bound proves the grace period was honored rather than SIGKILL being
/// sent immediately. A lower bound is immune to machine load by construction:
/// contention can only delay a process, never make it finish early.
#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_kill_when_term_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("trap-installed");
    let ctx = test_ctx();
    let start = std::time::Instant::now();
    let outcome = execute::run(&term_ignoring_command(&ready), &ctx, TRAP_SETUP_TIMEOUT_MS)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        ready.exists(),
        "precondition: the child never reached its `trap` builtin before the \
         {TRAP_SETUP_TIMEOUT_MS} ms deadline, so this run exercised plain \
         SIGTERM rather than the escalation path — raise TRAP_SETUP_TIMEOUT_MS"
    );
    assert!(outcome.timed_out, "expected timed_out = true");
    assert_eq!(
        outcome.signal,
        Some(libc::SIGKILL),
        "a child ignoring SIGTERM must be reaped by SIGKILL; signal = {:?}, \
         exit_status = {} (Some(15) means the escalation never happened; None/0 \
         means the child was never reaped at all)",
        outcome.signal,
        outcome.exit_status
    );
    let floor = std::time::Duration::from_millis(TRAP_SETUP_TIMEOUT_MS) + execute::TERMINATE_GRACE;
    assert!(
        elapsed >= floor,
        "SIGKILL must not pre-empt the SIGTERM grace period: elapsed {elapsed:?} \
         < deadline + TERMINATE_GRACE {floor:?}"
    );
}

/// stdout exceeding 30 KB must be middle-truncated and contain the marker.
#[cfg(unix)]
#[tokio::test]
async fn bash_truncation_stdout_above_30kb() {
    let ctx = test_ctx();
    // Generate 60 000 bytes of 'y\n' on stdout.
    let outcome = execute::run("yes | head -c 60000", &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert!(
        outcome.stdout.len() <= 30 * 1024,
        "stdout.len()={} > 30720",
        outcome.stdout.len()
    );
    assert!(
        String::from_utf8_lossy(&outcome.stdout).contains("[output truncated:"),
        "truncation marker not found in stdout"
    );
}

// --- middle_truncate unit tests ---

#[test]
fn middle_truncate_short_input_returned_verbatim() {
    let data = b"hello world";
    let result = execute::middle_truncate(data, 100);
    assert_eq!(result, data);
}

#[test]
fn middle_truncate_exact_budget_returned_verbatim() {
    let data = b"abcde";
    let result = execute::middle_truncate(data, 5);
    assert_eq!(result, data);
}

#[test]
fn middle_truncate_long_input_within_budget() {
    let data = vec![b'x'; 2_000];
    let result = execute::middle_truncate(&data, 1_000);
    assert!(result.len() <= 1_000, "len={}", result.len());
    let s = String::from_utf8_lossy(&result);
    assert!(s.contains("[output truncated:"), "marker missing");
}

#[test]
fn middle_truncate_marker_contains_byte_count() {
    let data = vec![b'a'; 10_000];
    let result = execute::middle_truncate(&data, 1_000);
    let s = String::from_utf8_lossy(&result);
    assert!(s.contains("bytes elided"), "marker text missing: {s}");
}

// --- split_leading_cd unit tests ---

#[test]
fn split_leading_cd_unquoted() {
    let (path, rest) = execute::split_leading_cd("cd /tmp && ls");
    assert_eq!(path, Some("/tmp"));
    assert_eq!(rest, "ls");
}

#[test]
fn split_leading_cd_double_quoted() {
    let (path, rest) = execute::split_leading_cd(r#"cd "with space" && ls"#);
    assert_eq!(path, Some("with space"));
    assert_eq!(rest, "ls");
}

#[test]
fn split_leading_cd_single_quoted() {
    let (path, rest) = execute::split_leading_cd("cd 'with space' && ls");
    assert_eq!(path, Some("with space"));
    assert_eq!(rest, "ls");
}

#[test]
fn split_leading_cd_semicolon() {
    let (path, rest) = execute::split_leading_cd("cd /tmp ; ls");
    assert_eq!(path, Some("/tmp"));
    assert_eq!(rest, "ls");
}

#[test]
fn split_leading_cd_no_match() {
    let cmd = "ls /tmp";
    let (path, rest) = execute::split_leading_cd(cmd);
    assert_eq!(path, None);
    assert_eq!(rest, cmd);
}

#[test]
fn split_leading_cd_no_match_when_cd_in_middle() {
    let cmd = "echo hi && cd /tmp";
    let (path, rest) = execute::split_leading_cd(cmd);
    assert_eq!(path, None);
    assert_eq!(rest, cmd);
}

// --- cancellation ---

/// SIGTERM path: fire cancel after 100 ms.
///
/// `outcome.cancelled` is only a flag, set by the code that requests
/// termination; it is not evidence anything died. `signal` is: a `run` that
/// flagged the cancel and then sat through the full `sleep 30` would report
/// `signal: None` and `exit_status: 0`, because an undisturbed `sleep` exits
/// normally.
#[cfg(unix)]
#[tokio::test]
async fn bash_cancellation_term() {
    let ctx = test_ctx();
    let cancel = ctx.cancel.clone();
    let handle = tokio::spawn(async move { execute::run("sleep 30", &ctx, DEFAULT_TIMEOUT).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    let outcome = handle.await.unwrap().unwrap();
    assert!(outcome.cancelled, "expected cancelled = true");
    assert_eq!(
        outcome.signal,
        Some(libc::SIGTERM),
        "cancellation must reap the child with SIGTERM; signal = {:?}, \
         exit_status = {} (None/0 means the cancel was flagged but `run` waited \
         out the sleep)",
        outcome.signal,
        outcome.exit_status
    );
}

/// SIGKILL fallback: cancellation must escalate past the grace period because
/// the child ignores SIGTERM.
///
/// Cancel fires only once the child has confirmed its trap is installed, rather
/// than after a fixed sleep. Reaching the `trap` builtin was measured at up to
/// 192 ms under load, so a cancel sent on a timer can land while the child still
/// holds the default disposition — killing it with plain SIGTERM and skipping
/// the escalation this test is named for. An upper bound on elapsed time cannot
/// detect that, because the skipped path is the faster one.
///
/// Two claims, neither of which a wall clock could separate: `signal` proves
/// SIGKILL — not SIGTERM — was the reaper, and the elapsed **lower** bound
/// proves the grace period was honored rather than SIGKILL being sent at once.
/// A lower bound cannot be failed by a busy machine, since contention only ever
/// delays a process.
#[cfg(unix)]
#[tokio::test]
async fn bash_cancellation_kill_when_term_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("trap-installed");
    let ctx = test_ctx();
    let cancel = ctx.cancel.clone();
    let cmd = term_ignoring_command(&ready);
    let handle = tokio::spawn(async move { execute::run(&cmd, &ctx, DEFAULT_TIMEOUT).await });
    await_trap_installed(&ready).await;
    let start = std::time::Instant::now();
    cancel.cancel();
    let outcome = handle.await.unwrap().unwrap();
    let elapsed = start.elapsed();
    assert!(outcome.cancelled, "expected cancelled = true");
    assert_eq!(
        outcome.signal,
        Some(libc::SIGKILL),
        "a child ignoring SIGTERM must be reaped by SIGKILL; signal = {:?}, \
         exit_status = {} (Some(15) means the escalation never happened; None/0 \
         means the child was never reaped at all)",
        outcome.signal,
        outcome.exit_status
    );
    assert!(
        elapsed >= execute::TERMINATE_GRACE,
        "SIGKILL must not pre-empt the SIGTERM grace period: elapsed {elapsed:?} \
         < TERMINATE_GRACE {:?}",
        execute::TERMINATE_GRACE
    );
}

/// Fire cancel after a naturally-exited command — should be a no-op.
#[cfg(unix)]
#[tokio::test]
async fn bash_cancellation_after_natural_exit_is_noop() {
    let ctx = test_ctx();
    let cancel = ctx.cancel.clone();
    let outcome = execute::run("true", &ctx, DEFAULT_TIMEOUT).await.unwrap();
    cancel.cancel();
    assert!(!outcome.cancelled, "expected cancelled = false");
    assert_eq!(outcome.exit_status, 0);
}

// --- env passthrough + pipefail injection ---
//
// Tests that mutate the process-global environment (std::env::set_var) must NOT
// run in parallel with other env-reading tests. Use #[serial_test::serial] so
// cargo-test serialises these within the process.

#[serial_test::serial]
#[test]
fn build_env_strips_ao_prefix() {
    std::env::set_var("AO_FOO", "secret");
    let env = execute::build_env();
    std::env::remove_var("AO_FOO");
    assert!(
        !env.iter().any(|(k, _)| k == "AO_FOO"),
        "AO_FOO should be stripped from subprocess env"
    );
}

#[serial_test::serial]
#[test]
fn build_env_strips_launchpad_prefix() {
    std::env::set_var("LAUNCHPAD_SECRET", "x");
    let env = execute::build_env();
    std::env::remove_var("LAUNCHPAD_SECRET");
    assert!(
        !env.iter().any(|(k, _)| k == "LAUNCHPAD_SECRET"),
        "LAUNCHPAD_SECRET should be stripped"
    );
}

#[serial_test::serial]
#[test]
fn build_env_strips_claude_prefix() {
    std::env::set_var("CLAUDE_KEY", "x");
    let env = execute::build_env();
    std::env::remove_var("CLAUDE_KEY");
    assert!(
        !env.iter().any(|(k, _)| k == "CLAUDE_KEY"),
        "CLAUDE_KEY should be stripped"
    );
}

#[test]
fn build_env_keeps_path() {
    let env = execute::build_env();
    assert!(
        env.iter().any(|(k, _)| k == "PATH"),
        "PATH should be retained in the subprocess env"
    );
}

#[test]
fn build_env_injects_bash_env() {
    let env = execute::build_env();
    let bash_env_val = env
        .iter()
        .find(|(k, _)| k == "BASH_ENV")
        .map(|(_, v)| v.to_str().unwrap().to_string());
    assert!(bash_env_val.is_some(), "BASH_ENV should be injected");
    let content = std::fs::read_to_string(bash_env_val.unwrap())
        .expect("BASH_ENV path should point to a readable file");
    assert!(
        content.contains("set -o pipefail"),
        "BASH_ENV file should contain 'set -o pipefail'"
    );
}

/// The BASH_ENV file must enable alias expansion so that aliases sourced from the
/// shell snapshot are usable in non-interactive `bash -c` subprocesses.
#[test]
fn build_env_bash_env_enables_alias_expansion() {
    let env = execute::build_env();
    let bash_env_val = env
        .iter()
        .find(|(k, _)| k == "BASH_ENV")
        .map(|(_, v)| v.to_str().unwrap().to_string());
    assert!(bash_env_val.is_some(), "BASH_ENV must be present");
    let content = std::fs::read_to_string(bash_env_val.unwrap())
        .expect("BASH_ENV must point to a readable file");
    assert!(
        content.contains("expand_aliases"),
        "BASH_ENV file must enable expand_aliases for alias expansion in non-interactive shells;\
         got:\n{content}"
    );
}

/// With pipefail injected via BASH_ENV, `false | true` exits 1 (the `false` failure
/// propagates). Without pipefail the pipeline exits 0 because `true` is last.
#[cfg(unix)]
#[tokio::test]
async fn bash_pipefail_visible() {
    let ctx = test_ctx();
    let outcome = execute::run("false | true; echo $?", &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    assert!(
        stdout.trim_start().starts_with("1"),
        "expected stdout to start with '1' (pipefail active), got: {stdout:?}"
    );
}

#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn bash_internal_env_not_leaked() {
    std::env::set_var("AO_INTERNAL", "secret");
    let ctx = test_ctx();
    let outcome = execute::run(r#"echo "${AO_INTERNAL:-MISSING}""#, &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    std::env::remove_var("AO_INTERNAL");
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    assert!(
        stdout.contains("MISSING"),
        "AO_INTERNAL should not be visible in subprocess, got: {stdout:?}"
    );
}

// --- cd integration tests (pwd-readback) ---

#[cfg(unix)]
#[tokio::test]
async fn bash_cd_lifted_runs_in_target() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();
    // Canonicalize so macOS /tmp -> /private/tmp symlink is resolved.
    let canonical = tokio::fs::canonicalize(&tmp_path).await.unwrap();
    let canonical_str = canonical.to_string_lossy().to_string();

    let ctx = test_ctx();
    let input = json!({ "command": format!("cd {} && pwd", canonical_str) });
    let result = BashTool.invoke(input, &ctx).await.unwrap();
    let text = result.as_text();
    assert!(
        text.contains(&canonical_str),
        "expected stdout to contain {canonical_str}, got: {text}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_cd_target_does_not_exist_rejected() {
    use serde_json::json;

    let ctx = test_ctx();
    let input = json!({ "command": "cd /this/path/does/not/exist/12345 && pwd" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();
    // With pwd-readback the cd runs inside the shell; a nonexistent target causes
    // the shell to exit non-zero (no pre-check ToolOutput::Error).
    match &result {
        ToolOutput::Structured(payload) => {
            assert_ne!(
                payload.get("exit_status").and_then(|v| v.as_i64()).unwrap_or(0),
                0,
                "cd to a nonexistent path must produce a non-zero exit status"
            );
        }
        other => panic!("expected Structured payload, got: {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_cd_target_is_file_rejected() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join("somefile.txt");
    std::fs::write(&file_path, b"data").unwrap();
    let canonical = tokio::fs::canonicalize(&file_path).await.unwrap();

    let ctx = test_ctx();
    let input = json!({ "command": format!("cd {} && pwd", canonical.to_string_lossy()) });
    let result = BashTool.invoke(input, &ctx).await.unwrap();
    // With pwd-readback the cd runs inside the shell; cd to a file fails non-zero.
    match &result {
        ToolOutput::Structured(payload) => {
            assert_ne!(
                payload.get("exit_status").and_then(|v| v.as_i64()).unwrap_or(0),
                0,
                "cd to a file path must produce a non-zero exit status"
            );
        }
        other => panic!("expected Structured payload, got: {other:?}"),
    }
}

// --- ToolOutput::Structured payload + render_text ---

#[cfg(unix)]
#[tokio::test]
async fn bash_structured_payload_shape() {
    use serde_json::json;
    let ctx = test_ctx();
    let input = json!({ "command": "echo hi 1>&2; echo bye; exit 7" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(payload) => {
            assert_eq!(payload["stderr"], "hi\n", "stderr mismatch");
            assert_eq!(payload["stdout"], "bye\n", "stdout mismatch");
            assert_eq!(payload["exit_status"], 7, "exit_status mismatch");
            assert_eq!(payload["cancelled"], false, "cancelled should be false");
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

/// render_text: stdout lines verbatim, then stderr lines prefixed, then footer.
/// For `echo a; echo b 1>&2; echo c`, stdout="a\nc\n" and stderr="b\n".
#[cfg(unix)]
#[tokio::test]
async fn bash_render_text_interleave() {
    use serde_json::json;
    let ctx = test_ctx();
    let input = json!({ "command": "echo a; echo b 1>&2; echo c" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            let text = super::render_text(payload);
            assert_eq!(
                text, "a\nc\nstderr: b\nexit=0\n",
                "render_text mismatch: {text:?}"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

#[test]
fn bash_render_text_signal_precedence() {
    use serde_json::json;
    let payload = json!({
        "stdout": "",
        "stderr": "",
        "exit_status": 0,
        "signal": 15_i64,
        "timed_out": false,
        "cancelled": false,
    });
    let text = super::render_text(&payload);
    assert!(text.ends_with("signal=15\n"), "footer mismatch: {text:?}");
}

#[test]
fn bash_render_text_cancelled_precedence() {
    use serde_json::json;
    let payload = json!({
        "stdout": "",
        "stderr": "",
        "exit_status": 0,
        "signal": serde_json::Value::Null,
        "timed_out": true,
        "cancelled": true,
    });
    let text = super::render_text(&payload);
    assert!(text.ends_with("cancelled\n"), "footer mismatch: {text:?}");
}

// --- check_permissions injects classification into Ask reason ---

#[tokio::test]
async fn bash_check_permissions_git_push_git_mutating() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "git push origin main"});
    let decision = tool.check_permissions(&input, &ctx).await;
    match decision {
        PermissionDecision::Ask { reason } => {
            assert!(
                reason.contains("[classification: GitMutating]"),
                "expected GitMutating in reason, got: {reason}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_check_permissions_ls_read_only() {
    // ls is on the auto-approve allowlist and classifies ReadOnly → returns Allow.
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "ls /tmp"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Allow),
        "expected Allow for safe ls, got: {decision:?}"
    );
}

#[tokio::test]
async fn bash_check_permissions_cd_lifted_before_classify() {
    // cd lifting strips `cd /tmp &&` before classifying — `git push` after the cd
    // should still yield GitMutating, not Unclassified from the path.
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "cd /tmp && git push origin main"});
    let decision = tool.check_permissions(&input, &ctx).await;
    match decision {
        PermissionDecision::Ask { reason } => {
            assert!(
                reason.contains("[classification: GitMutating]"),
                "expected GitMutating after cd-lift, got: {reason}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_check_permissions_missing_command_unclassified() {
    // Input with no command field → empty string → Unclassified
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({});
    let decision = tool.check_permissions(&input, &ctx).await;
    match decision {
        PermissionDecision::Ask { reason } => {
            assert!(
                reason.contains("[classification: Unclassified]"),
                "expected Unclassified for missing command, got: {reason}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

// --- description surfaced in Ask reason ---

#[tokio::test]
async fn bash_check_permissions_description_included_in_reason() {
    // `find` classifies ReadOnly but is excluded from auto-approve (exec escape risk),
    // so description-carrying Ask is still exercised on the non-auto-approved path.
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({
        "command": "find . -type f",
        "description": "List files to verify directory contents"
    });
    let decision = tool.check_permissions(&input, &ctx).await;
    match decision {
        PermissionDecision::Ask { reason } => {
            assert!(
                reason.contains("List files to verify directory contents"),
                "expected description in reason, got: {reason}"
            );
            assert!(
                reason.contains("[classification: ReadOnly]"),
                "expected classification in reason, got: {reason}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_check_permissions_absent_description_no_stray_separator() {
    // `find` is ReadOnly but NOT auto-approved, so the Ask path runs.
    // Verifies the reason format has no stray separators when description is absent.
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({ "command": "find . -type f" });
    let decision = tool.check_permissions(&input, &ctx).await;
    match decision {
        PermissionDecision::Ask { reason } => {
            assert_eq!(
                reason,
                "[classification: ReadOnly] execute bash: find . -type f",
                "reason format changed when description is absent: {reason}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_check_permissions_empty_description_no_stray_separator() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({ "command": "find . -type f", "description": "" });
    let decision = tool.check_permissions(&input, &ctx).await;
    match decision {
        PermissionDecision::Ask { reason } => {
            assert_eq!(
                reason,
                "[classification: ReadOnly] execute bash: find . -type f",
                "empty description must not inject stray separator: {reason}"
            );
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

// --- auto-approve Allow / Ask decisions ---

#[tokio::test]
async fn auto_approve_ls_la_returns_allow() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "ls -la"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Allow),
        "expected Allow for ls -la, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_git_status_returns_allow() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "git status"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Allow),
        "expected Allow for git status, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_grep_pipe_head_returns_allow() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "grep -r foo src/ | head"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Allow),
        "expected Allow for grep | head pipeline, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_cd_and_cat_returns_allow() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "cd /tmp && cat file"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Allow),
        "expected Allow for cd /tmp && cat file, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_rm_rf_returns_ask() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "rm -rf /tmp/x"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Ask { .. }),
        "expected Ask for rm -rf, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_echo_with_command_substitution_returns_ask() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "echo $(rm -rf /)"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Ask { .. }),
        "expected Ask for echo with command substitution, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_chained_ls_rm_returns_ask() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "ls && rm -rf /"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Ask { .. }),
        "expected Ask for ls && rm chain, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_git_push_returns_ask() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "git push origin main"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Ask { .. }),
        "expected Ask for git push, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_curl_returns_ask() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "curl http://example.com"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Ask { .. }),
        "expected Ask for curl, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_find_delete_returns_ask() {
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "find . -delete"});
    let decision = tool.check_permissions(&input, &ctx).await;
    assert!(
        matches!(decision, PermissionDecision::Ask { .. }),
        "expected Ask for find -delete, got: {decision:?}"
    );
}

#[tokio::test]
async fn auto_approve_ask_preserves_description_and_classification() {
    // Non-auto-approvable command with a description — verifies the Ask reason
    // still embeds the description and classification tag on the non-approved path.
    let tool = BashTool;
    let ctx = PermissionContext::new(PermissionMode::Default, "agent", "sess");
    let input = serde_json::json!({"command": "rm -rf /tmp/x", "description": "clean tmp"});
    let decision = tool.check_permissions(&input, &ctx).await;
    match decision {
        PermissionDecision::Ask { reason } => {
            assert!(
                reason.contains("clean tmp"),
                "expected description in reason, got: {reason}"
            );
            assert!(
                reason.contains("[classification:"),
                "expected classification tag in reason, got: {reason}"
            );
        }
        other => panic!("expected Ask, got: {other:?}"),
    }
}

// --- is_error propagation ---

/// A timed-out command must produce a payload with is_error = true.
#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_sets_is_error() {
    use serde_json::json;
    let ctx = test_ctx();
    // Use a subshell invocation so the sleep guard (which blocks bare `sleep N`)
    // does not intercept this command. The subshell still takes 10 s, timing out
    // after 200 ms as intended.
    let result = BashTool.invoke(json!({ "command": "bash -c 'sleep 10'", "timeout": 200 }), &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            assert!(
                payload.get("timed_out").and_then(|v| v.as_bool()).unwrap_or(false),
                "expected timed_out=true"
            );
            assert_eq!(
                payload.get("is_error").and_then(|v| v.as_bool()),
                Some(true),
                "timed-out command must have is_error=true in payload"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

/// A normally-completing command with non-zero exit must NOT set is_error.
#[cfg(unix)]
#[tokio::test]
async fn bash_nonzero_exit_does_not_set_is_error() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool.invoke(json!({ "command": "exit 3" }), &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            let is_err = payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            assert!(!is_err, "non-zero exit must not set is_error, payload: {payload}");
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

/// A cancelled command must produce a payload with is_error = true.
#[cfg(unix)]
#[tokio::test]
async fn bash_cancellation_sets_is_error() {
    let ctx = test_ctx();
    let cancel = ctx.cancel.clone();
    let handle = tokio::spawn(async move {
        execute::run("sleep 30", &ctx, DEFAULT_TIMEOUT).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();
    let outcome = handle.await.unwrap().unwrap();
    assert!(outcome.cancelled, "expected cancelled=true");
    // Verify the is_error field value matches the cancelled || timed_out contract.
    let is_error = outcome.cancelled || outcome.timed_out;
    assert!(is_error, "is_error must be true for a cancelled command");
}

// --- background mode ---

#[cfg(unix)]
#[tokio::test]
async fn bash_background_returns_process_id() {
    use serde_json::json;

    let ctx = test_ctx();
    let input = json!({ "command": "sleep 30", "run_in_background": true });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    // Must return Structured output with process_id, status, command, output_path.
    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    let process_id_str = payload["process_id"]
        .as_str()
        .expect("process_id must be a string");

    // IDs are short human-friendly strings like "bash_1", "bash_2".
    assert!(
        process_id_str.starts_with("bash_"),
        "process_id must start with 'bash_', got: {process_id_str}"
    );
    let suffix = &process_id_str["bash_".len()..];
    assert!(
        suffix.parse::<u64>().is_ok(),
        "process_id suffix must be a positive integer, got: {process_id_str}"
    );

    assert_eq!(
        payload["status"].as_str(),
        Some("running"),
        "status must be 'running'"
    );
    assert_eq!(
        payload["command"].as_str(),
        Some("sleep 30"),
        "command must match"
    );

    let output_path_str = payload["output_path"]
        .as_str()
        .expect("output_path must be a string");
    assert!(
        output_path_str.ends_with(".log"),
        "output_path must be a .log file, got: {output_path_str}"
    );

    // The command should be registered in the context's background command registry.
    let ids = ctx.background_commands.list().await;
    assert_eq!(ids.len(), 1, "registry should contain exactly one entry");
}

// --- Bash leading-cd persists ctx.cwd ---

/// After `cd /tmp && echo ok`, ctx.cwd must reflect the new directory.
/// /tmp may canonicalize to /private/tmp on macOS — we compare against
/// tokio::fs::canonicalize so the test is portable.
#[cfg(unix)]
#[tokio::test]
async fn bash_leading_cd_updates_ctx_cwd() {
    let canonical_tmp = tokio::fs::canonicalize("/tmp").await.unwrap();
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/"));
    let input = serde_json::json!({ "command": "cd /tmp && echo ok" });
    let out = BashTool.invoke(input, &ctx).await.unwrap();
    assert!(
        !matches!(out, ToolOutput::Error { .. }),
        "command should succeed"
    );
    let new_cwd = ctx.cwd.read().unwrap().clone();
    assert_eq!(
        new_cwd, canonical_tmp,
        "ctx.cwd should update to canonical /tmp"
    );
}

/// Commands without a leading `cd` must not change ctx.cwd.
#[cfg(unix)]
#[tokio::test]
async fn bash_no_leading_cd_preserves_ctx_cwd() {
    let start = PathBuf::from("/tmp");
    let ctx = RunnerContext::new_with_cwd("sess", "agent", start.clone());
    let input = serde_json::json!({ "command": "echo hello" });
    BashTool.invoke(input, &ctx).await.unwrap();
    let cwd = ctx.cwd.read().unwrap().clone();
    assert_eq!(
        cwd, start,
        "ctx.cwd must not change for commands without a leading cd"
    );
}

/// When ctx.cwd shares an Arc with an external container (simulating the
/// McpAgentSession.cwd Arc sharing), a Bash leading-cd write propagates to
/// both references automatically.
#[cfg(unix)]
#[tokio::test]
async fn bash_leading_cd_propagates_via_shared_arc() {
    use std::sync::{Arc, RwLock};

    let canonical_tmp = tokio::fs::canonicalize("/tmp").await.unwrap();

    // Simulate session.cwd: an Arc the "session store" holds.
    let session_cwd: Arc<RwLock<PathBuf>> = Arc::new(RwLock::new(PathBuf::from("/")));

    // RunnerContext shares the same Arc (mirrors NativeAgentRunner::run).
    let ctx = RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/"))
        .with_cwd_arc(Arc::clone(&session_cwd));

    let input = serde_json::json!({ "command": "cd /tmp && echo ok" });
    BashTool.invoke(input, &ctx).await.unwrap();

    // Both the context and the "session" Arc should now hold the canonical /tmp.
    let ctx_cwd = ctx.cwd.read().unwrap().clone();
    let sess_cwd = session_cwd.read().unwrap().clone();
    assert_eq!(ctx_cwd, canonical_tmp, "ctx.cwd should be /tmp");
    assert_eq!(
        sess_cwd, canonical_tmp,
        "shared session Arc should also be /tmp"
    );
}

// --- large-output persistence ---

/// Output below the persistence threshold must be returned inline (existing
/// middle-truncation path). 60 KB is below the 100 KB persistence threshold
/// and above the 30 KB truncation limit, so the marker must be present.
#[cfg(unix)]
#[tokio::test]
async fn bash_sub_threshold_output_returned_inline_with_truncation() {
    let ctx = test_ctx();
    // Generate 60 000 bytes — above the 30 KB truncation limit but below 100 KB.
    let outcome = execute::run("yes | head -c 60000", &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert!(
        !outcome.needs_persistence,
        "60 KB should not trigger persistence (threshold is 100 KB)"
    );
    assert!(
        String::from_utf8_lossy(&outcome.stdout).contains("[output truncated:"),
        "60 KB stdout should be middle-truncated with marker"
    );
}

/// Output above the persistence threshold must set `needs_persistence = true`
/// and return untruncated bytes (no middle-truncation marker).
#[cfg(unix)]
#[tokio::test]
async fn bash_above_threshold_sets_needs_persistence() {
    let ctx = test_ctx();
    // 150 000 bytes exceeds the 100 KB persistence threshold.
    let outcome = execute::run("yes | head -c 150000", &ctx, DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert!(
        outcome.needs_persistence,
        "150 KB should set needs_persistence = true"
    );
    assert!(
        !String::from_utf8_lossy(&outcome.stdout).contains("[output truncated:"),
        "persisted output must not contain the truncation marker"
    );
    assert!(
        outcome.stdout.len() > 100_000,
        "persisted stdout should be untruncated (>100 KB)"
    );
}

/// End-to-end: invoking Bash with large output writes a file to disk and
/// returns a `<persisted-output>` envelope in the result.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn bash_large_output_persisted_to_disk() {
    use serde_json::json;

    // Redirect the data root to a temp directory so the test is self-contained.
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    // 150 000 bytes of 'y\n' exceeds the 100 KB persistence threshold.
    let input = json!({ "command": "yes | head -c 150000" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // The payload must carry the persisted-output path.
    let path_str = payload
        .get("persisted_output_path")
        .and_then(|v| v.as_str())
        .expect("persisted_output_path must be present");

    // The file must exist on disk.
    let path = std::path::Path::new(path_str);
    assert!(path.exists(), "persisted output file must exist: {path_str}");

    // The file contents must be the full untruncated output (no truncation marker).
    let file_contents = std::fs::read_to_string(path).unwrap();
    assert!(
        !file_contents.contains("[output truncated:"),
        "persisted file must not contain the truncation marker"
    );
    assert!(
        file_contents.len() > 100_000,
        "persisted file must hold the full output (>100 KB), got {} bytes",
        file_contents.len()
    );

    // Byte size in payload must match the actual file size.
    let reported_size = payload
        .get("persisted_output_size")
        .and_then(|v| v.as_u64())
        .expect("persisted_output_size must be present");
    assert_eq!(
        reported_size,
        file_contents.len() as u64,
        "reported size must match actual file size"
    );

    // Line count in payload must match the actual number of lines in the file.
    let reported_lines = payload
        .get("persisted_output_lines")
        .and_then(|v| v.as_u64())
        .expect("persisted_output_lines must be present");
    let actual_lines = file_contents.lines().count() as u64;
    assert_eq!(
        reported_lines, actual_lines,
        "reported line count ({reported_lines}) must match actual file line count ({actual_lines})"
    );

    // The text_fallback must contain the <persisted-output> envelope.
    let text_fallback = payload
        .get("text_fallback")
        .and_then(|v| v.as_str())
        .expect("text_fallback must be present for persisted output");
    assert!(
        text_fallback.contains("<persisted-output"),
        "text_fallback must contain the envelope tag"
    );
    assert!(
        text_fallback.contains(path_str),
        "envelope must embed the filepath"
    );
    assert!(
        text_fallback.contains("bytes"),
        "envelope must contain the size in bytes"
    );
}

/// `as_text()` — the path the query loop uses to build provider tool results —
/// must surface the `<persisted-output>` envelope for large outputs.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn bash_large_output_as_text_returns_envelope() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    let input = json!({ "command": "yes | head -c 150000" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let text = result.as_text();
    assert!(
        text.contains("<persisted-output"),
        "as_text() must surface the envelope tag, got: {text:.200}"
    );
    assert!(
        text.contains("bytes"),
        "as_text() must include byte size in envelope"
    );
}

/// The head preview in the envelope reflects the first 20 lines; the tail
/// preview reflects the last 20. Uses `seq` to produce numbered lines.
/// `seq 1 30000` → ~165 KB (5.5 bytes/line average), above the 100 KB threshold.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn bash_large_output_envelope_head_and_tail_preview() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    let input = json!({ "command": "seq 1 30000" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // Verify persistence was triggered.
    assert!(
        payload.get("persisted_output_path").is_some(),
        "seq 1 30000 should trigger persistence"
    );

    let text_fallback = payload
        .get("text_fallback")
        .and_then(|v| v.as_str())
        .expect("text_fallback must be present");

    assert!(text_fallback.contains("--- head"), "envelope must have head section");
    assert!(text_fallback.contains("--- tail"), "envelope must have tail section");

    // "1" is the first line of seq output; it must appear in the head preview.
    assert!(
        text_fallback.contains("head (20 lines) ---\n1\n"),
        "head must start with line '1'; got: {text_fallback:.500}"
    );
    // "30000" is the last line; it must appear in the tail preview.
    assert!(
        text_fallback.contains("30000"),
        "tail must contain last line '30000'"
    );
}

/// Regression guard: output below the persistence threshold must not produce a
/// `<persisted-output>` envelope or write any file to disk when going through
/// the full `BashTool::invoke` path.
///
/// Uses 60 KB (above the 30 KB truncation limit, below the 100 KB persistence
/// threshold) so the middle-truncation path is exercised instead of persistence.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn bash_invoke_sub_threshold_no_persisted_envelope() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    let input = json!({ "command": "yes | head -c 60000" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    assert!(
        payload.get("persisted_output_path").is_none(),
        "sub-threshold output must not include persisted_output_path in payload"
    );
    // `text_fallback` is present on every Bash payload, so its absence cannot
    // discriminate the inline path from the persisted one. The invariant is
    // asserted directly instead: inline output means the bytes travel in the
    // payload rather than behind a file path.
    assert!(
        payload.get("stdout").and_then(|v| v.as_str()).is_some(),
        "sub-threshold output must carry stdout inline in the payload"
    );
    let text_fallback = payload
        .get("text_fallback")
        .and_then(|v| v.as_str())
        .expect("every payload carries a rendering");
    assert!(
        !text_fallback.contains("<persisted-output"),
        "sub-threshold rendering must be the inline form: {text_fallback:.200}"
    );

    let text = result.as_text();
    assert!(
        !text.contains("<persisted-output"),
        "sub-threshold output must not produce a <persisted-output> envelope; got: {text:.200}"
    );

    let bash_output_dir = tmp.path().join("bash-output");
    assert!(
        !bash_output_dir.exists(),
        "bash-output dir must not be created for sub-threshold output"
    );
}

// --- background command registry + disk output + bounded buffers ---

/// A short background command gets a valid human-friendly id, a disk file is
/// created, the file receives the command's output, and the status transitions
/// to Exited with the correct exit code.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn background_command_id_disk_output_and_status() {
    use ao_engine_tools_core::BackgroundCommandStatus;
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    let input = json!({ "command": "echo bgtest", "run_in_background": true });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // ID must be short human-friendly format.
    let id_str = payload["process_id"].as_str().expect("process_id string");
    assert!(
        id_str.starts_with("bash_"),
        "id must start with 'bash_', got: {id_str}"
    );

    // Disk output path must be present and point to a .log file.
    let output_path_str = payload["output_path"].as_str().expect("output_path string");
    let output_path = std::path::Path::new(output_path_str);
    assert!(output_path_str.ends_with(".log"), "output_path must end with .log");

    // Give the drain task time to write output and update status.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // File must exist and contain the command's output.
    assert!(output_path.exists(), "disk output file must exist: {output_path_str}");
    let contents = std::fs::read_to_string(output_path).unwrap();
    assert!(
        contents.contains("bgtest"),
        "disk output must contain 'bgtest', got: {contents:?}"
    );

    // In-memory buffer must also contain the output.
    let ids = ctx.background_commands.list().await;
    assert_eq!(ids.len(), 1, "registry must have one entry");
    let handle = ctx.background_commands.get(&ids[0]).await.unwrap();
    let buf = handle.output_buffer.lock().unwrap();
    let buf_str = String::from_utf8_lossy(buf.as_bytes());
    assert!(
        buf_str.contains("bgtest"),
        "in-memory buffer must contain 'bgtest', got: {buf_str:?}"
    );
    drop(buf);

    // Status must have transitioned to Exited with code 0.
    let status = handle.status.lock().unwrap().clone();
    assert_eq!(
        status,
        BackgroundCommandStatus::Exited { code: 0 },
        "status must be Exited{{code:0}}, got: {status:?}"
    );
}

/// A failing background command (exit 7) must produce status Exited{code:7}.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn background_command_nonzero_exit_code() {
    use ao_engine_tools_core::BackgroundCommandStatus;
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    let input = json!({ "command": "exit 7", "run_in_background": true });
    BashTool.invoke(input, &ctx).await.unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let ids = ctx.background_commands.list().await;
    assert_eq!(ids.len(), 1, "registry must have one entry");
    let handle = ctx.background_commands.get(&ids[0]).await.unwrap();
    let status = handle.status.lock().unwrap().clone();
    assert_eq!(
        status,
        BackgroundCommandStatus::Exited { code: 7 },
        "status must be Exited{{code:7}}, got: {status:?}"
    );
}

/// The bounded ring buffer caps at OUTPUT_BUFFER_CAP bytes; writing more than
/// the cap must not grow the buffer beyond it, and dropped_bytes must be > 0.
#[test]
fn bounded_output_buffer_cap_enforced() {
    use ao_engine_tools_core::BoundedOutputBuffer;

    let cap = 1024usize;
    let mut buf = BoundedOutputBuffer::new(cap);

    // Write more than the cap in two batches.
    let chunk_a = vec![b'a'; 800];
    let chunk_b = vec![b'b'; 800];
    buf.append(&chunk_a);
    buf.append(&chunk_b);

    assert!(
        buf.len() <= cap,
        "buffer must not exceed cap: len={} cap={cap}",
        buf.len()
    );
    assert!(
        buf.dropped_bytes > 0,
        "dropped_bytes must be > 0 after overflow"
    );
}

/// A single chunk larger than the cap must be capped to exactly `capacity` bytes.
#[test]
fn bounded_output_buffer_oversized_chunk() {
    use ao_engine_tools_core::BoundedOutputBuffer;

    let cap = 512usize;
    let mut buf = BoundedOutputBuffer::new(cap);

    let big = vec![b'x'; cap * 3];
    buf.append(&big);

    assert_eq!(buf.len(), cap, "buffer must be exactly cap bytes after oversized append");
    assert!(buf.dropped_bytes > 0, "dropped_bytes must reflect elided bytes");
}

// --- image output detection ---

/// Minimal 1×1 white PNG in base64 — used across image output tests.
const TINY_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// A command that prints raw base64 PNG to stdout must produce an image content block.
#[cfg(unix)]
#[tokio::test]
async fn bash_image_output_raw_base64_png() {
    use ao_engine_tools_core::ToolBlock;
    use base64::Engine as _;
    use serde_json::json;

    let ctx = test_ctx();
    let input = json!({ "command": format!("printf '%s' '{TINY_PNG_B64}'") });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    match result {
        ToolOutput::Blocks(ref blocks) => {
            assert_eq!(blocks.len(), 1, "expected a single image block");
            match &blocks[0] {
                ToolBlock::Image { media_type, data } => {
                    assert_eq!(media_type, "image/png");
                    // Verify decoded bytes start with the PNG magic header.
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .expect("image block must carry valid base64");
                    assert!(
                        decoded.starts_with(b"\x89PNG\r\n\x1a\n"),
                        "decoded bytes must begin with PNG magic"
                    );
                }
                other => panic!("expected Image block, got: {other:?}"),
            }
        }
        other => panic!("expected Blocks for PNG output, got: {other:?}"),
    }
}

/// A command that prints a data-URI form must produce an image content block with
/// the correct media type stripped from the prefix.
#[cfg(unix)]
#[tokio::test]
async fn bash_image_output_data_uri_form() {
    use ao_engine_tools_core::ToolBlock;
    use serde_json::json;

    let ctx = test_ctx();
    let uri = format!("data:image/png;base64,{TINY_PNG_B64}");
    let input = json!({ "command": format!("printf '%s' '{uri}'") });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    match result {
        ToolOutput::Blocks(ref blocks) => {
            assert_eq!(blocks.len(), 1, "expected a single image block");
            match &blocks[0] {
                ToolBlock::Image { media_type, data } => {
                    assert_eq!(media_type, "image/png", "media type from data-URI prefix");
                    // The data portion must NOT include the data-URI prefix.
                    assert!(
                        !data.starts_with("data:"),
                        "data field must not include the data-URI prefix"
                    );
                    assert_eq!(data, TINY_PNG_B64, "data must equal the stripped base64");
                }
                other => panic!("expected Image block, got: {other:?}"),
            }
        }
        other => panic!("expected Blocks for data-URI output, got: {other:?}"),
    }
}

/// A command that prints ordinary text must NOT produce an image block.
#[cfg(unix)]
#[tokio::test]
async fn bash_image_output_plain_text_not_detected() {
    use serde_json::json;

    let ctx = test_ctx();
    let input = json!({ "command": "echo 'hello world'" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    assert!(
        !matches!(result, ToolOutput::Blocks(_)),
        "plain text output must not produce an image block"
    );
}

/// Cancelled or timed-out commands must NOT be treated as images even if stdout
/// happens to contain base64 data.
#[cfg(unix)]
#[tokio::test]
async fn bash_image_output_skipped_on_error() {
    use serde_json::json;

    let ctx = test_ctx();
    // The command prints valid base64 PNG then exits; but we force is_error via timeout.
    // We use a very short timeout on a command that should not complete in time.
    let input = json!({
        "command": format!("printf '%s' '{TINY_PNG_B64}'; sleep 10"),
        "timeout": 200
    });
    let result = BashTool.invoke(input, &ctx).await.unwrap();

    // The timed-out result must be Structured (error path), not Blocks.
    assert!(
        matches!(result, ToolOutput::Structured(_)),
        "timed-out command must not produce an image block"
    );
}

// --- exit code interpretation (unit tests) ---

#[test]
fn interpret_exit_0_no_note() {
    assert!(
        super::interpret_exit_code(0, None).is_none(),
        "exit 0 must produce no note"
    );
}

#[test]
fn interpret_exit_127_command_not_found() {
    let note = super::interpret_exit_code(127, None).expect("exit 127 must produce a note");
    assert!(note.contains("127"), "note must mention exit code: {note}");
    assert!(
        note.to_lowercase().contains("not found") || note.to_lowercase().contains("path"),
        "note must reference PATH or not found: {note}"
    );
}

#[test]
fn interpret_exit_126_not_executable() {
    let note = super::interpret_exit_code(126, None).expect("exit 126 must produce a note");
    assert!(note.contains("126"), "note must mention exit code: {note}");
    assert!(
        note.to_lowercase().contains("executable") || note.to_lowercase().contains("permission"),
        "note must reference executable/permission: {note}"
    );
}

#[test]
fn interpret_exit_130_sigint() {
    let note = super::interpret_exit_code(130, None).expect("exit 130 must produce a note");
    assert!(
        note.contains("130") || note.to_lowercase().contains("sigint") || note.to_lowercase().contains("interrupt"),
        "note must reference SIGINT or interrupt: {note}"
    );
}

#[test]
fn interpret_exit_137_sigkill() {
    let note = super::interpret_exit_code(137, None).expect("exit 137 must produce a note");
    assert!(
        note.contains("137") || note.to_lowercase().contains("sigkill") || note.to_lowercase().contains("kill"),
        "note must reference SIGKILL: {note}"
    );
}

#[test]
fn interpret_exit_143_sigterm() {
    let note = super::interpret_exit_code(143, None).expect("exit 143 must produce a note");
    assert!(
        note.contains("143") || note.to_lowercase().contains("sigterm"),
        "note must reference SIGTERM: {note}"
    );
}

#[test]
fn interpret_exit_other_nonzero_generic_note() {
    let note = super::interpret_exit_code(42, None).expect("exit 42 must produce a generic note");
    assert!(!note.is_empty(), "generic note must be non-empty");
}

#[test]
fn interpret_signal_15_sigterm() {
    let note = super::interpret_exit_code(0, Some(15)).expect("signal 15 must produce a note");
    assert!(
        note.to_lowercase().contains("sigterm") || note.to_lowercase().contains("15"),
        "note must reference SIGTERM: {note}"
    );
}

#[test]
fn interpret_signal_9_sigkill() {
    let note = super::interpret_exit_code(0, Some(9)).expect("signal 9 must produce a note");
    assert!(
        note.to_lowercase().contains("sigkill") || note.to_lowercase().contains("9"),
        "note must reference SIGKILL: {note}"
    );
}

// --- exit code note in structured payload (integration) ---

#[cfg(unix)]
#[tokio::test]
async fn bash_exit_127_note_in_payload() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool.invoke(json!({ "command": "command_xyz_notfound_28479" }), &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            assert_eq!(payload["exit_status"], 127_i64, "expected exit 127 for command not found");
            let note = payload.get("exit_code_note").and_then(|v| v.as_str()).unwrap_or("");
            assert!(!note.is_empty(), "exit 127 must produce a non-empty exit_code_note");
            assert!(
                note.to_lowercase().contains("not found") || note.to_lowercase().contains("path"),
                "note should reference PATH or not found: {note}"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_exit_126_note_in_payload() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool.invoke(json!({ "command": "exit 126" }), &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            assert_eq!(payload["exit_status"], 126_i64, "expected exit 126");
            let note = payload.get("exit_code_note").and_then(|v| v.as_str()).unwrap_or("");
            assert!(!note.is_empty(), "exit 126 must produce a non-empty exit_code_note");
            assert!(
                note.to_lowercase().contains("executable") || note.to_lowercase().contains("permission"),
                "note should reference executable/permission: {note}"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_exit_130_note_in_payload() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool.invoke(json!({ "command": "exit 130" }), &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            assert_eq!(payload["exit_status"], 130_i64, "expected exit 130");
            let note = payload.get("exit_code_note").and_then(|v| v.as_str()).unwrap_or("");
            assert!(!note.is_empty(), "exit 130 must produce a non-empty exit_code_note");
            assert!(
                note.to_lowercase().contains("sigint") || note.to_lowercase().contains("interrupt") || note.contains("130"),
                "note should reference SIGINT: {note}"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_exit_137_note_in_payload() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool.invoke(json!({ "command": "exit 137" }), &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            assert_eq!(payload["exit_status"], 137_i64, "expected exit 137");
            let note = payload.get("exit_code_note").and_then(|v| v.as_str()).unwrap_or("");
            assert!(!note.is_empty(), "exit 137 must produce a non-empty exit_code_note");
            assert!(
                note.to_lowercase().contains("sigkill") || note.to_lowercase().contains("kill") || note.contains("137"),
                "note should reference SIGKILL: {note}"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_exit_0_no_note_in_payload() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool.invoke(json!({ "command": "true" }), &ctx).await.unwrap();
    match result {
        ToolOutput::Structured(ref payload) => {
            assert_eq!(payload["exit_status"], 0_i64, "expected exit 0");
            assert!(
                payload.get("exit_code_note").is_none()
                    || payload.get("exit_code_note").and_then(|v| v.as_str()) == Some(""),
                "exit 0 must not produce an exit_code_note"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

// --- sleep guard ---

#[cfg(unix)]
#[tokio::test]
async fn bash_bare_sleep_blocked_recoverable_error() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool.invoke(json!({ "command": "sleep 30" }), &ctx).await.unwrap();
    assert!(
        matches!(&result, ToolOutput::Error { message, recoverable: true } if message.to_lowercase().contains("sleep")),
        "bare foreground sleep must return a recoverable error mentioning sleep, got: {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_bare_sleep_threshold_blocked() {
    use serde_json::json;
    let ctx = test_ctx();
    // Exactly at threshold (2 s) — should be blocked.
    let result = BashTool.invoke(json!({ "command": "sleep 2" }), &ctx).await.unwrap();
    assert!(
        matches!(&result, ToolOutput::Error { recoverable: true, .. }),
        "sleep 2 must be blocked (at threshold), got: {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_short_sleep_not_blocked() {
    use serde_json::json;
    let ctx = test_ctx();
    // Below threshold — must NOT be blocked.
    let result = BashTool.invoke(json!({ "command": "sleep 1" }), &ctx).await.unwrap();
    assert!(
        !matches!(result, ToolOutput::Error { .. }),
        "sleep 1 must not be blocked (below threshold), got: {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_sleep_in_pipeline_not_blocked() {
    use serde_json::json;
    let ctx = test_ctx();
    // Combined with other tokens — detect_bare_sleep returns None.
    let result = BashTool.invoke(json!({ "command": "sleep 1 && echo done" }), &ctx).await.unwrap();
    assert!(
        !matches!(result, ToolOutput::Error { .. }),
        "sleep followed by other tokens must not be blocked, got: {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_sleep_in_background_not_blocked() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    let ctx = test_ctx();
    // run_in_background=true bypasses the sleep guard entirely.
    let result = BashTool.invoke(
        json!({ "command": "sleep 30", "run_in_background": true }),
        &ctx,
    ).await.unwrap();
    drop(tmp);
    assert!(
        !matches!(result, ToolOutput::Error { .. }),
        "sleep with run_in_background must not be blocked, got: {result:?}"
    );
}

// --- auto-backgrounding ---
//
// AUTO_BG_THRESHOLD_MS = 3 s in test builds, so a sleep-based command that lasts
// more than 3 s is auto-backgrounded without waiting the 15 s production value.
// Sleep durations below are calibrated against that 3 s figure; changing the
// constant requires revisiting them.

/// A foreground command that outlasts the auto-background threshold must return
/// a structured payload with a bash_N process_id and an `auto_backgrounded` flag,
/// NOT an is_error result.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn auto_bg_long_command_returns_process_id_not_error() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    // bash -c 'sleep 30' bypasses the bare-sleep guard and runs longer than the 3 s threshold.
    let result = BashTool
        .invoke(json!({ "command": "bash -c 'sleep 30'" }), &ctx)
        .await
        .unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // Must carry auto_backgrounded flag.
    assert_eq!(
        payload.get("auto_backgrounded").and_then(|v| v.as_bool()),
        Some(true),
        "payload must have auto_backgrounded=true"
    );

    // Must NOT be an error.
    assert!(
        !payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
        "auto-backgrounded command must not set is_error"
    );

    // Must carry a valid bash_N process id.
    let id_str = payload["process_id"].as_str().expect("process_id must be a string");
    assert!(
        id_str.starts_with("bash_"),
        "process_id must start with 'bash_', got: {id_str}"
    );
    let suffix = &id_str["bash_".len()..];
    assert!(
        suffix.parse::<u64>().is_ok(),
        "process_id suffix must be a positive integer, got: {id_str}"
    );

    // Must carry a disk output path.
    let output_path_str = payload["output_path"].as_str().expect("output_path must be a string");
    assert!(
        output_path_str.ends_with(".log"),
        "output_path must be a .log file, got: {output_path_str}"
    );

    // Note field must mention BashStatus and BashKill.
    let note = payload["note"].as_str().unwrap_or("");
    assert!(
        note.contains("BashStatus") && note.contains("BashKill"),
        "note must reference BashStatus and BashKill, got: {note}"
    );

    // Process must appear in the registry.
    let ids = ctx.background_commands.list().await;
    assert_eq!(ids.len(), 1, "one entry must be in the registry after auto-bg");
}

/// After auto-backgrounding, BashStatus shows Running immediately and then
/// Exited once the command finishes naturally.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn auto_bg_bash_status_shows_running_then_exited() {
    use ao_engine_tools_core::BackgroundCommandStatus;
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    // sleep 4 outlasts the 3 s threshold, so it is auto-backgrounded first, then
    // exits ~1 s later while the test observes the Running -> Exited transition.
    let result = BashTool
        .invoke(json!({ "command": "bash -c 'sleep 4'" }), &ctx)
        .await
        .unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };
    assert_eq!(
        payload.get("auto_backgrounded").and_then(|v| v.as_bool()),
        Some(true),
        "expected auto_backgrounded=true"
    );

    let id_str = payload["process_id"].as_str().unwrap().to_string();

    // Immediately after registration the process must still be Running.
    let ids = ctx.background_commands.list().await;
    assert_eq!(ids.len(), 1, "registry must have one entry");
    let handle = ctx.background_commands.get(&ids[0]).await.unwrap();
    assert_eq!(
        *handle.status.lock().unwrap(),
        BackgroundCommandStatus::Running,
        "status must be Running immediately after auto-backgrounding"
    );

    // Wait for the sleep to finish and the drain task to update status. The sleep
    // has ~1 s left at this point; the remainder is margin for a loaded machine.
    tokio::time::sleep(std::time::Duration::from_millis(3_000)).await;

    let status = handle.status.lock().unwrap().clone();
    assert_eq!(
        status,
        BackgroundCommandStatus::Exited { code: 0 },
        "status must be Exited{{code:0}} after natural exit; process_id={id_str}, got: {status:?}"
    );
}

/// A fast command (completes before the threshold) must return a normal payload
/// without the auto_backgrounded field.
#[cfg(unix)]
#[tokio::test]
async fn auto_bg_fast_command_unaffected() {
    use serde_json::json;
    let ctx = test_ctx();
    let result = BashTool
        .invoke(json!({ "command": "echo hello" }), &ctx)
        .await
        .unwrap();

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // Fast commands must not be auto-backgrounded.
    assert!(
        !payload.get("auto_backgrounded").and_then(|v| v.as_bool()).unwrap_or(false),
        "fast commands must not have auto_backgrounded=true"
    );
    // Must carry normal stdout.
    let stdout = payload.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        stdout.contains("hello"),
        "fast command stdout must contain 'hello', got: {stdout:?}"
    );
    // Must not be an error.
    assert_eq!(
        payload.get("is_error").and_then(|v| v.as_bool()),
        Some(false),
        "fast command must not be an error"
    );
}

/// Output produced before the auto-background threshold must appear in both the
/// disk log file and the in-memory buffer after the handoff.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn auto_bg_pre_threshold_output_preserved() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    // Print a marker immediately, then sleep past the threshold.
    // The shell is used explicitly so the sleep guard doesn't intercept.
    let result = BashTool
        .invoke(
            json!({ "command": "echo MARKER_PRE; bash -c 'sleep 30'" }),
            &ctx,
        )
        .await
        .unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };
    assert_eq!(
        payload.get("auto_backgrounded").and_then(|v| v.as_bool()),
        Some(true),
        "expected auto_backgrounded=true"
    );

    // Give the drain task a moment to write the prelude to disk.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let output_path_str = payload["output_path"].as_str().unwrap();
    let output_path = std::path::Path::new(output_path_str);
    assert!(output_path.exists(), "disk log must exist: {output_path_str}");

    let disk_content = std::fs::read_to_string(output_path).unwrap();
    assert!(
        disk_content.contains("MARKER_PRE"),
        "disk log must contain pre-threshold output 'MARKER_PRE', got: {disk_content:?}"
    );

    // In-memory buffer must also contain the marker (pre-seeded at registration).
    let ids = ctx.background_commands.list().await;
    assert_eq!(ids.len(), 1, "registry must have one entry");
    let handle = ctx.background_commands.get(&ids[0]).await.unwrap();
    let buf = handle.output_buffer.lock().unwrap();
    let buf_str = String::from_utf8_lossy(buf.as_bytes());
    assert!(
        buf_str.contains("MARKER_PRE"),
        "in-memory buffer must contain 'MARKER_PRE', got: {buf_str:?}"
    );
}

/// An explicit timeout (set by the caller) must still kill the command at the
/// specified time and must NOT trigger auto-backgrounding.
#[cfg(unix)]
#[tokio::test]
async fn explicit_timeout_bypasses_auto_bg() {
    use serde_json::json;
    let ctx = test_ctx();
    // Short explicit timeout of 200ms; command would otherwise run 30s.
    let result = BashTool
        .invoke(
            json!({ "command": "bash -c 'sleep 30'", "timeout": 200 }),
            &ctx,
        )
        .await
        .unwrap();

    let payload = match &result {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // Must NOT be auto-backgrounded.
    assert!(
        !payload.get("auto_backgrounded").and_then(|v| v.as_bool()).unwrap_or(false),
        "explicit-timeout command must not be auto-backgrounded"
    );
    // Must be a timeout error.
    assert_eq!(
        payload.get("timed_out").and_then(|v| v.as_bool()),
        Some(true),
        "expected timed_out=true for explicit-timeout path"
    );
    assert_eq!(
        payload.get("is_error").and_then(|v| v.as_bool()),
        Some(true),
        "explicit-timeout path must set is_error=true"
    );

    // Registry must be empty (no background registration).
    let ids = ctx.background_commands.list().await;
    assert!(
        ids.is_empty(),
        "explicit-timeout path must not register any background command, got: {ids:?}"
    );
}

/// Multiple sequential background commands each get unique ids.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn background_command_ids_are_unique() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    BashTool.invoke(json!({ "command": "true", "run_in_background": true }), &ctx).await.unwrap();
    BashTool.invoke(json!({ "command": "true", "run_in_background": true }), &ctx).await.unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let ids = ctx.background_commands.list().await;
    assert_eq!(ids.len(), 2, "two commands must be registered");

    let id_strs: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    assert_ne!(id_strs[0], id_strs[1], "each command must have a unique id");
}

// --- editor / pager / credential neutralizers ---

#[serial_test::serial]
#[test]
fn build_env_injects_editor_and_pager_neutralizers() {
    let env = execute::build_env();
    let get = |key: &str| -> Option<String> {
        env.iter()
            .find(|(k, _)| k.to_str() == Some(key))
            .map(|(_, v)| v.to_str().unwrap_or("").to_string())
    };
    assert_eq!(get("GIT_EDITOR").as_deref(), Some("true"), "GIT_EDITOR");
    assert_eq!(get("EDITOR").as_deref(), Some("true"), "EDITOR");
    assert_eq!(get("VISUAL").as_deref(), Some("true"), "VISUAL");
    assert_eq!(get("GIT_SEQUENCE_EDITOR").as_deref(), Some("true"), "GIT_SEQUENCE_EDITOR");
    assert_eq!(get("GIT_PAGER").as_deref(), Some("cat"), "GIT_PAGER");
    assert_eq!(get("PAGER").as_deref(), Some("cat"), "PAGER");
    assert_eq!(get("GIT_TERMINAL_PROMPT").as_deref(), Some("0"), "GIT_TERMINAL_PROMPT");
}

#[serial_test::serial]
#[test]
fn build_env_overrides_inherited_editor() {
    std::env::set_var("EDITOR", "vim");
    let env = execute::build_env();
    std::env::remove_var("EDITOR");

    let matches: Vec<_> = env
        .iter()
        .filter(|(k, _)| k.to_str() == Some("EDITOR"))
        .collect();
    assert_eq!(matches.len(), 1, "EDITOR must appear exactly once");
    assert_eq!(
        matches[0].1.to_str(),
        Some("true"),
        "inherited EDITOR=vim must be overridden to 'true'"
    );
}

/// Verify `git commit` without `-m` does not hang when run with the neutralized env.
/// Skipped gracefully when `git` is unavailable.
#[cfg(unix)]
#[test]
fn build_env_git_commit_no_hang() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    let git_setup = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("HOME", tmp.path())
            .output()
            .expect("git setup command failed to spawn")
    };

    git_setup(&["init"]);
    git_setup(&["config", "user.email", "t@t.com"]);
    git_setup(&["config", "user.name", "T"]);
    std::fs::write(repo.join("f"), b"v1").unwrap();
    git_setup(&["add", "f"]);
    git_setup(&["commit", "-m", "init"]);
    std::fs::write(repo.join("f"), b"v2").unwrap();
    git_setup(&["add", "f"]);

    let env = execute::build_env();
    let start = Instant::now();
    let mut child = std::process::Command::new("git")
        .arg("commit")
        .current_dir(repo)
        .env_clear()
        .envs(env)
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("git commit failed to spawn");

    let deadline = start + Duration::from_secs(5);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("git commit without -m hung past the 5-second deadline");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- prompt guidance presence (sleep guard) ---

/// The tool DESCRIPTION must document the bare-sleep blocking threshold so the
/// model knows which patterns are restricted before attempting them.
#[test]
fn bash_description_contains_sleep_guidance() {
    let desc = prompt::DESCRIPTION;
    // Must mention the 2 s threshold.
    assert!(
        desc.contains("2 s") || desc.contains("2s"),
        "DESCRIPTION must mention the 2-second bare-sleep threshold"
    );
    // Must recommend run_in_background as the alternative.
    assert!(
        desc.contains("run_in_background"),
        "DESCRIPTION must recommend run_in_background for long sleeps"
    );
    // Must name BashStatus as the polling tool.
    assert!(
        desc.contains("BashStatus"),
        "DESCRIPTION must name BashStatus for polling background processes"
    );
}

// --- render_text with exit_code_note ---

/// render_text must append the exit_code_note after the footer when the payload
/// carries one, with a trailing newline.
#[test]
fn bash_render_text_appends_exit_code_note() {
    use serde_json::json;
    let payload = json!({
        "stdout": "output\n",
        "stderr": "",
        "exit_status": 127_i64,
        "signal": serde_json::Value::Null,
        "timed_out": false,
        "cancelled": false,
        "is_error": false,
        "exit_code_note": "Exit 127: command not found — the executable is missing from PATH or the name is misspelled."
    });
    let text = super::render_text(&payload);
    assert!(
        text.contains("exit=127"),
        "footer must appear before note: {text:?}"
    );
    assert!(
        text.contains("Exit 127: command not found"),
        "exit_code_note must appear in render_text output: {text:?}"
    );
    // Note must come after the footer line.
    let footer_pos = text.find("exit=127").unwrap();
    let note_pos = text.find("Exit 127").unwrap();
    assert!(
        note_pos > footer_pos,
        "note must appear after the exit footer: {text:?}"
    );
}

/// When exit_code_note is absent, render_text must not produce a stray blank line
/// after the footer.
#[test]
fn bash_render_text_no_note_no_trailing_blank() {
    use serde_json::json;
    let payload = json!({
        "stdout": "ok\n",
        "stderr": "",
        "exit_status": 0_i64,
        "signal": serde_json::Value::Null,
        "timed_out": false,
        "cancelled": false,
        "is_error": false,
    });
    let text = super::render_text(&payload);
    // Must end with exactly "exit=0\n" and nothing after.
    assert_eq!(
        text, "ok\nexit=0\n",
        "no-note payload must end cleanly after footer: {text:?}"
    );
}

// --- render_text with auto_backgrounded payload ---

// --- pwd-readback CWD persistence ---

/// mkdir + cd in one command — ctx.cwd updates to the newly created directory.
/// The old single-leading-cd approach could not handle this because `mkdir x && cd x`
/// uses a relative path not visible before the shell runs.
#[cfg(unix)]
#[tokio::test]
async fn bash_mkdir_and_cd_persists_new_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let canonical_tmp = tokio::fs::canonicalize(tmp.path()).await.unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent", canonical_tmp.clone());
    let input = serde_json::json!({ "command": "mkdir -p newdir && cd newdir" });
    let result = BashTool.invoke(input, &ctx).await.unwrap();
    assert!(!matches!(result, ToolOutput::Error { .. }), "mkdir + cd must succeed");

    let new_cwd = ctx.cwd.read().unwrap().clone();
    assert_eq!(
        new_cwd,
        canonical_tmp.join("newdir"),
        "ctx.cwd must update to the newly created directory"
    );
}

/// `cd a && cd b` — only the final directory persists.
/// The old single-leading-cd approach peeled only the first cd, leaving the second lost.
#[cfg(unix)]
#[tokio::test]
async fn bash_multi_cd_persists_final_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let canonical_tmp = tokio::fs::canonicalize(tmp.path()).await.unwrap();
    std::fs::create_dir(canonical_tmp.join("aa")).unwrap();
    std::fs::create_dir(canonical_tmp.join("bb")).unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent", canonical_tmp.clone());
    let aa = canonical_tmp.join("aa").to_string_lossy().to_string();
    let bb = canonical_tmp.join("bb").to_string_lossy().to_string();
    let result = BashTool
        .invoke(serde_json::json!({ "command": format!("cd {aa} && cd {bb}") }), &ctx)
        .await
        .unwrap();
    assert!(!matches!(result, ToolOutput::Error { .. }), "multi-cd must succeed");

    let new_cwd = ctx.cwd.read().unwrap().clone();
    assert_eq!(
        new_cwd,
        canonical_tmp.join("bb"),
        "ctx.cwd must reflect the final cd destination, not the intermediate one"
    );
}

/// The cwd-capture wrapper must not alter the user command's exit status.
#[cfg(unix)]
#[tokio::test]
async fn bash_cwd_wrapper_preserves_exit_status() {
    let ctx = test_ctx();

    let o = execute::run("false", &ctx, DEFAULT_TIMEOUT).await.unwrap();
    assert_eq!(o.exit_status, 1, "false must exit 1 through the cwd wrapper");

    let o = execute::run("true", &ctx, DEFAULT_TIMEOUT).await.unwrap();
    assert_eq!(o.exit_status, 0, "true must exit 0 through the cwd wrapper");

    let o = execute::run("cd /tmp && false", &ctx, DEFAULT_TIMEOUT).await.unwrap();
    assert_eq!(o.exit_status, 1, "cd && false must preserve false's exit code");
}

/// When ctx.cwd points to a deleted directory, a command still spawns successfully
/// (in the nearest existing ancestor) and ctx.cwd is updated to that ancestor.
#[cfg(unix)]
#[tokio::test]
async fn bash_deleted_cwd_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let canonical_tmp = tokio::fs::canonicalize(tmp.path()).await.unwrap();
    let gone = canonical_tmp.join("gone");
    std::fs::create_dir(&gone).unwrap();

    let ctx = RunnerContext::new_with_cwd("sess", "agent", gone.clone());

    // Delete the directory after wiring it into ctx.
    std::fs::remove_dir(&gone).unwrap();

    // Command must succeed despite the deleted cwd.
    let outcome = execute::run("echo ok", &ctx, DEFAULT_TIMEOUT).await.unwrap();
    assert_eq!(outcome.exit_status, 0, "command must succeed after cwd recovery");
    assert!(
        String::from_utf8_lossy(&outcome.stdout).contains("ok"),
        "stdout must contain 'ok'"
    );

    // ctx.cwd must now point to an existing directory.
    let recovered = ctx.cwd.read().unwrap().clone();
    assert!(recovered.exists(), "recovered cwd must exist: {recovered:?}");
    assert_eq!(
        recovered, canonical_tmp,
        "recovered cwd must be the nearest existing ancestor of the deleted dir"
    );
}

/// render_text for an auto_backgrounded payload must produce a compact summary
/// with the process_id, output_path, and the descriptive note on separate lines.
#[test]
fn bash_render_text_auto_backgrounded_format() {
    use serde_json::json;
    let payload = json!({
        "process_id": "bash_7",
        "status": "running",
        "command": "bash -c 'sleep 30'",
        "output_path": "/tmp/.launchpad/bash-background/bash_7.log",
        "auto_backgrounded": true,
        "note": "Command ran past the 15s limit and was moved to the background. Poll with BashStatus(\"bash_7\") or stop with BashKill(\"bash_7\").",
    });
    let text = super::render_text(&payload);
    assert!(
        text.contains("process_id=bash_7"),
        "must contain process_id line: {text:?}"
    );
    assert!(
        text.contains("output_path="),
        "must contain output_path line: {text:?}"
    );
    assert!(
        text.contains("BashStatus"),
        "note must reference BashStatus: {text:?}"
    );
    assert!(
        text.contains("BashKill"),
        "note must reference BashKill: {text:?}"
    );
    // Must end with a newline.
    assert!(
        text.ends_with('\n'),
        "auto_backgrounded render must end with newline: {text:?}"
    );
}

// --- reachability: the rendering actually reaches the model ------------------
//
// Every other render_text test above calls super::render_text directly, so all
// of them would still pass if nothing in the live path ever invoked it. These
// two go through BashTool::invoke and then ToolOutput::as_text() — the exact
// conversion query_loop, the MCP bridge, and the CLI renderer each apply to a
// structured result — so they fail if the payload stops carrying the rendering.

/// An ordinary Bash result must reach the model as the flat rendering, not as
/// the JSON serialization of the payload.
#[cfg(unix)]
#[tokio::test]
async fn bash_invoke_result_reaches_the_model_as_flat_text() {
    use serde_json::json;

    let ctx = test_ctx();
    let out = BashTool
        .invoke(
            json!({ "command": "echo hello; echo oops >&2; exit 3" }),
            &ctx,
        )
        .await
        .unwrap();

    let payload = match &out {
        ToolOutput::Structured(v) => v.clone(),
        other => panic!("expected Structured, got: {other:?}"),
    };

    // as_text() is what the transports call. It must agree with render_text.
    let model_facing = out.as_text();
    assert_eq!(
        model_facing,
        super::render_text(&payload),
        "the text transports produce must be the Bash rendering"
    );

    // The discriminating assertion: without the rendering in the payload,
    // structured_to_text falls back to the JSON form, which begins with '{'
    // and escapes the newlines in stdout.
    assert!(
        !model_facing.starts_with('{'),
        "model must not receive the raw JSON payload: {model_facing:?}"
    );
    assert!(
        model_facing.starts_with("hello\n"),
        "stdout must lead, unescaped: {model_facing:?}"
    );
    assert!(
        model_facing.contains("stderr: oops\n"),
        "stderr lines must be prefixed: {model_facing:?}"
    );
    assert!(
        model_facing.contains("exit=3"),
        "exit footer must be present: {model_facing:?}"
    );
    // The note tells the model to inspect the output "above" — true only in
    // this ordering. In the JSON form serde sorts exit_code_note before stderr
    // and stdout, so the note would refer to text that follows it.
    let note_at = model_facing
        .find("Non-zero exit status")
        .expect("exit_code_note must reach the model");
    assert!(
        note_at > model_facing.find("stderr: oops").unwrap(),
        "note must follow the output it refers to: {model_facing:?}"
    );
}

/// An auto-backgrounded result must reach the model as the compact summary
/// rather than as JSON.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn auto_bg_result_reaches_the_model_as_flat_text() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    // Must outlast AUTO_BG_THRESHOLD_MS (3 s in test builds) to be promoted.
    // Kept just above it rather than at 30 s: the spawned process outlives this
    // test, and several tests in this file assert wall-clock bounds that a
    // loaded machine already threatens.
    let out = BashTool
        .invoke(json!({ "command": "bash -c 'sleep 5'" }), &ctx)
        .await
        .unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let model_facing = out.as_text();
    assert!(
        !model_facing.starts_with('{'),
        "model must not receive the raw JSON payload: {model_facing:?}"
    );
    assert!(
        model_facing.starts_with("process_id=bash_"),
        "compact summary must lead with the process id: {model_facing:?}"
    );
    assert!(
        model_facing.contains("BashStatus") && model_facing.contains("BashKill"),
        "summary must name the follow-up tools: {model_facing:?}"
    );
}

/// An explicit `run_in_background: true` spawn must reach the model as the
/// background summary, not as `exit=0`.
///
/// This payload carries no `exit_status`, `signal`, `timed_out` or `cancelled`
/// field, so a renderer that fell through to the exit-footer path would report
/// a still-running process as cleanly exited — the failure this guards.
#[cfg(unix)]
#[serial_test::serial]
#[tokio::test]
async fn explicit_background_result_reaches_the_model_as_flat_text() {
    use serde_json::json;

    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

    let ctx = test_ctx();
    let out = BashTool
        .invoke(
            // Short-lived: the payload is built at spawn time, so the process
            // only has to be alive long enough to register.
            json!({ "command": "sleep 2", "run_in_background": true }),
            &ctx,
        )
        .await
        .unwrap();

    std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");

    let model_facing = out.as_text();
    assert!(
        !model_facing.starts_with('{'),
        "model must not receive the raw JSON payload: {model_facing:?}"
    );
    assert!(
        model_facing.starts_with("process_id=bash_"),
        "summary must lead with the process id: {model_facing:?}"
    );
    assert!(
        model_facing.contains("status=running"),
        "summary must state the process is running: {model_facing:?}"
    );
    assert!(
        !model_facing.contains("exit="),
        "a running process must not be reported as exited: {model_facing:?}"
    );
    // No note is produced on this path; the rendering must not leave a stray
    // blank line where one would have gone.
    assert!(
        !model_facing.contains("\n\n"),
        "no blank line without a note: {model_facing:?}"
    );
}
