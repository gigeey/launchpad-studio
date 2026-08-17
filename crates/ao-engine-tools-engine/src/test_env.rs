use std::path::Path;
use std::sync::MutexGuard;

use ao_protocol::data_root::DATA_DIR_ENV_VAR;
use tempfile::TempDir;

/// RAII guard that pins `LAUNCHPAD_STUDIO_DATA_DIR` to a fresh tempdir for
/// the duration of one test and restores the prior value on drop.
///
/// Every test in this crate that reads or writes `LAUNCHPAD_STUDIO_DATA_DIR`
/// must hold a `DataDirGuard` for its full duration — including assertions.
/// The guard acquires the crate-wide [`crate::ENV_VAR_MUTEX`] so concurrent
/// tests cannot observe or clobber each other's override.
pub(crate) struct DataDirGuard {
    tmp: TempDir,
    prior: Option<String>,
    _lock: MutexGuard<'static, ()>,
}

impl DataDirGuard {
    pub(crate) fn new() -> Self {
        let lock = crate::lock_env_var();
        let tmp = tempfile::tempdir().expect("create data-root tempdir");
        let prior = std::env::var(DATA_DIR_ENV_VAR).ok();
        std::env::set_var(DATA_DIR_ENV_VAR, tmp.path());
        Self {
            tmp,
            prior,
            _lock: lock,
        }
    }

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
