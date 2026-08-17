//! Test-only coordination for the process-global data-root override.
//!
//! `cargo test` runs tests on parallel threads within a single process,
//! and `LAUNCHPAD_STUDIO_DATA_DIR` is process-global state. Any test that
//! reads or writes the variable must hold a [`DataDirGuard`] for its full
//! duration — including assertions — or a concurrently running test can
//! observe (or clobber) the override mid-flight.
//!
//! One test binary, one env namespace, one lock: every module in this
//! crate that touches the data-root variable must go through this guard
//! rather than rolling its own module-local lock (which would only
//! serialize against itself).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use ao_protocol::data_root::DATA_DIR_ENV_VAR;
use tempfile::TempDir;

/// Crate-wide lock serializing all tests that touch the data-root env var.
static DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that pins `LAUNCHPAD_STUDIO_DATA_DIR` to a fresh tempdir for
/// the duration of one test. Drop restores the previous value (or unsets
/// the variable if none was set), so a panicking test cannot leak its
/// tempdir path — or an unset state — into the next test or the developer's
/// shell-provided override.
pub(crate) struct DataDirGuard {
    tmp: TempDir,
    prior: Option<String>,
    _lock: MutexGuard<'static, ()>,
}

impl DataDirGuard {
    pub(crate) fn new() -> Self {
        // A poisoned lock just means a previous test panicked while holding
        // it; the env var is restored by that guard's Drop, so it is safe
        // to continue.
        let lock = DATA_DIR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().expect("create data-root tempdir");
        let prior = std::env::var(DATA_DIR_ENV_VAR).ok();
        std::env::set_var(DATA_DIR_ENV_VAR, tmp.path());
        Self {
            tmp,
            prior,
            _lock: lock,
        }
    }

    /// The tempdir currently serving as the data root.
    pub(crate) fn data_dir(&self) -> &Path {
        self.tmp.path()
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(DATA_DIR_ENV_VAR, v),
            None => std::env::remove_var(DATA_DIR_ENV_VAR),
        }
    }
}
