//! Shell configuration snapshot — captures rc-derived functions, aliases, and PATH
//! once per process so every Bash tool subprocess inherits the user's normal shell environment.
//!
//! The snapshot is a bash-sourceable file emitted by running `bash -lic` so that both
//! login files (`~/.bash_profile`, `/etc/profile`) and the interactive rc (`~/.bashrc`)
//! are processed.  It is sourced via the `BASH_ENV` mechanism, which bash reads for
//! every non-interactive invocation, before the user's command runs.
//!
//! Capture is best-effort: timeout, spawn failure, or empty output each produce a
//! `tracing::warn!` and a `None` return.  Command execution continues normally; the
//! subprocess just won't have rc-derived aliases and functions available.

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

/// Bash-sourceable file containing the user's shell functions, aliases, and PATH,
/// captured once from a login+interactive bash session.
///
/// `None` when capture fails or times out.  All consumers must treat `None` as
/// "proceed without snapshot" rather than a fatal error.
pub static SHELL_SNAPSHOT_FILE: LazyLock<Option<PathBuf>> = LazyLock::new(capture);

/// Capture the user's interactive shell environment into a persistent temp file.
fn capture() -> Option<PathBuf> {
    // The dump script produces bash-sourceable output:
    //   declare -f  → all function definitions
    //   alias       → all alias definitions (alias name='value' form)
    //   printf ...  → resolved PATH as an export statement
    //
    // Running with -lic triggers login (-l) and interactive (-i) startup so that
    // ~/.bash_profile and ~/.bashrc are sourced, matching a normal terminal session.
    let dump_script =
        "declare -f 2>/dev/null; alias 2>/dev/null; printf 'export PATH=%s\\n' \"$PATH\"";

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = std::process::Command::new("bash")
            .args(["-lic", dump_script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();
        let _ = tx.send(result);
    });

    let output = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!("shell config capture failed to spawn bash: {e}; rc-derived aliases/functions will not be available");
            return None;
        }
        Err(_elapsed) => {
            tracing::warn!("shell config capture timed out after 5 s; rc-derived aliases/functions will not be available");
            return None;
        }
    };

    if output.stdout.is_empty() {
        tracing::warn!("shell config capture produced no output; rc-derived aliases/functions will not be available");
        return None;
    }

    persist_snapshot(output.stdout)
}

/// Write `content` to a persistent temp file and register an atexit cleanup handler.
pub(crate) fn persist_snapshot(content: Vec<u8>) -> Option<PathBuf> {
    use std::io::Write;

    let mut f = match tempfile::Builder::new()
        .prefix("launchpad-shellsnap-")
        .tempfile_in(std::env::temp_dir())
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("shell snapshot file creation failed: {e}");
            return None;
        }
    };

    if let Err(e) = f.write_all(&content) {
        tracing::warn!("shell snapshot write failed: {e}");
        return None;
    }

    let (_, path) = match f.keep() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("shell snapshot persist failed: {e}");
            return None;
        }
    };

    // SAFETY: atexit is POSIX-standard; SHELL_SNAPSHOT_FILE is already initialized
    // when the callback fires (atexit is only registered from within this initializer).
    unsafe { libc::atexit(cleanup_snapshot) };

    Some(path)
}

extern "C" fn cleanup_snapshot() {
    if let Some(p) = &*SHELL_SNAPSHOT_FILE {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    /// Verifies that the BASH_ENV snapshot mechanism (alias expansion + sourcing a snapshot file)
    /// makes rc-defined aliases and shell functions available to bash subprocesses.
    ///
    /// This tests the exact mechanism that execute::run relies on via build_env(): a BASH_ENV file
    /// that enables expand_aliases and sources the snapshot is read by every `bash -c` subprocess
    /// before the user's command runs.  The synthetic snapshot is constructed in the test so the
    /// result is deterministic and does not depend on the real user's rc files.
    #[cfg(unix)]
    #[tokio::test]
    async fn alias_and_function_available_via_snapshot_mechanism() {
        // Build a synthetic snapshot with a known alias and a shell function.
        let mut snap = tempfile::Builder::new()
            .prefix("test-snap-")
            .tempfile()
            .unwrap();
        writeln!(snap, "alias say_hi='echo HI_FROM_ALIAS'").unwrap();
        writeln!(snap, "greet() {{ echo HI_FROM_FUNC; }}").unwrap();
        let (snap_f, snap_path) = snap.keep().unwrap();
        drop(snap_f);

        // Construct a BASH_ENV file in the same format as execute.rs writes for BASH_ENV_FILE.
        let mut env_file = tempfile::Builder::new()
            .prefix("test-bashenv-")
            .tempfile()
            .unwrap();
        writeln!(env_file, "set -o pipefail").unwrap();
        writeln!(env_file, "shopt -s expand_aliases 2>/dev/null || true").unwrap();
        writeln!(
            env_file,
            "[ -f \"{}\" ] && source \"{}\" 2>/dev/null || true",
            snap_path.display(),
            snap_path.display()
        )
        .unwrap();
        let (env_f, env_path) = env_file.keep().unwrap();
        drop(env_f);

        // Spawn bash -c with this BASH_ENV — the same invocation pattern as execute::run.
        let out = tokio::process::Command::new("/bin/bash")
            .args(["-c", "say_hi && greet"])
            .env_clear()
            .env("BASH_ENV", &env_path)
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .output()
            .await
            .unwrap();

        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("HI_FROM_ALIAS"),
            "alias must expand when BASH_ENV sources the snapshot; got stdout: {stdout:?}"
        );
        assert!(
            stdout.contains("HI_FROM_FUNC"),
            "function must be callable when BASH_ENV sources the snapshot; got stdout: {stdout:?}"
        );

        let _ = std::fs::remove_file(&snap_path);
        let _ = std::fs::remove_file(&env_path);
    }

    /// Smoke test: the static initializes without panicking in a test process.
    /// The value may be None in headless CI environments where bash rc files are absent.
    #[test]
    fn shell_snapshot_static_initializes_without_panic() {
        // Force initialization; result is intentionally not asserted (may be None in CI).
        let _ = &*super::SHELL_SNAPSHOT_FILE;
    }
}
