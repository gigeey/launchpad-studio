//! Permanent end-to-end proof that `propagate_keychain_forbidden` actually
//! reaches a spawned CHILD process, not just that the predicate it wraps
//! answers correctly in isolation.
//!
//! The bug this guards against: `ao_engine_tools_provider_config`'s
//! `keychain_forbidden()` correctly detects "am I a cargo test/bench
//! harness binary" by inspecting its own `current_exe()` — but that
//! detection is process-local. A real cargo test binary that spawns this
//! exact `ao-engine-tools-cli` binary as a child (see `cli_smoke.rs`) hits
//! the real OS keychain from *inside the child*, because the child's own
//! executable is an ordinary build artifact, not a test harness binary. A
//! unit test of `keychain_forbidden()` alone can never catch that class of
//! gap: it proves the predicate is correct, not that its answer reaches
//! every process that needs it before that process goes ahead and touches
//! the keychain. This test spawns a real child process and inspects what
//! the CHILD actually did, which a predicate-only test cannot.

use std::process::Command;

/// Write a minimal `providers.toml` with a plaintext API key — enough for
/// `ProviderConfig::load()` to run its "absorb the plaintext key into the
/// vault" step on startup, which is what actually touches the secret
/// backend (keychain or file) before the REPL loop ever starts.
fn write_providers_toml(dir: &std::path::Path) {
    let content = "[anthropic]\napi_key = \"sk-ant-SMOKE-TEST\"\nmodel = \"claude-opus-4-7\"\n";
    std::fs::write(dir.join("providers.toml"), content).unwrap();
}

/// Path to the compiled binary under test.
fn binary_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ao-engine-tools-cli"))
}

/// This test's own process is itself a cargo test binary, so
/// `keychain_forbidden()` already answers `true` for it — same as it does
/// for `cli_smoke`'s test binary. Calling `propagate_keychain_forbidden()`
/// reifies that into *this* process's own environment; the child below
/// inherits it purely through ordinary `std::process::Command` environment
/// inheritance. Nothing here passes an explicit `--env`/`.env(...)`
/// override naming the vault backend — that is the entire point: the child
/// is never told directly, it only ever inherits.
#[test]
fn keychain_forbidden_reaches_a_spawned_child_via_inherited_environment() {
    ao_engine_tools_provider_config::propagate_keychain_forbidden();

    let dir = tempfile::tempdir().unwrap();
    write_providers_toml(dir.path());

    // `wait_with_output` closes the child's stdin before waiting (see its
    // std docs), so the REPL's first `readline()` sees immediate EOF and
    // the process exits right after `ProviderConfig::load()` — no stub
    // server or network turn needed to observe the vault backend decision.
    let child = Command::new(binary_path())
        .arg("--provider")
        .arg("anthropic")
        .env("LAUNCHPAD_STUDIO_DATA_DIR", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn ao-engine-tools-cli");
    let output = child.wait_with_output().expect("child wait_with_output failed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "expected the child to exit cleanly; stderr:\n{stderr}");
    assert!(
        !stderr.to_lowercase().contains("keychain"),
        "the child must never mention a keychain error once its parent has propagated \
         keychain-forbidden into the environment it inherits; stderr:\n{stderr}"
    );

    // `ProviderConfig::load()` absorbs providers.toml's plaintext `api_key`
    // into the vault on every load. The real OS keychain backend would
    // store that in the keychain and never touch disk; only the file
    // backend writes `secret_vault.json`. Its presence is a black-box,
    // backend-agnostic witness that the child actually selected the file
    // backend — i.e. that it really did inherit `keychain_forbidden` from
    // its parent, rather than the parent's propagation call being a no-op
    // that happened to not crash anything.
    assert!(
        dir.path().join("secret_vault.json").exists(),
        "expected the child to select the file-backed vault (evidenced by secret_vault.json), \
         proving it inherited keychain_forbidden from its parent instead of reaching the real \
         OS keychain"
    );
}
