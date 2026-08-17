//! Shared test-only helpers for serializing access to process-global env vars.
//!
//! `LAUNCHPAD_STUDIO_DATA_DIR` (and a handful of per-store fallback/env-var
//! flags) are process-global state read by `ao_protocol::data_root` and by
//! this crate's stores. Every test module that points one of these at a
//! tempdir must acquire the *same* lock here — a per-module mutex only
//! serializes tests within that module, so two modules' tests running on
//! separate threads can still stomp on each other's env-var value.

use std::sync::{Mutex, MutexGuard};

static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Acquire the crate-wide env-var test lock, recovering from poison.
///
/// A test that panics while holding this lock must not cascade into
/// failures for every other test in the crate, so a poisoned lock is
/// treated the same as a healthy one rather than propagated via
/// `.unwrap()`.
pub(crate) fn lock_env() -> MutexGuard<'static, ()> {
    ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII guard that sets an env var and restores its prior value on drop —
/// including during an unwind — so a panicking test can't leave a mutated
/// process-global env var behind for the next test to observe.
pub(crate) struct EnvGuard {
    key: String,
    prior: Option<String>,
}

impl EnvGuard {
    pub(crate) fn set(key: &str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key: key.to_owned(), prior }
    }

    /// Removes `key` for the duration of the guard, restoring its prior
    /// value (if any) on drop. Lets a test prove behavior for an *absent*
    /// var regardless of what the ambient shell happens to have set —
    /// `CI` in particular is set by every real CI runner but normally
    /// absent from a local dev shell, so a test relying on "just don't set
    /// it" would pass locally yet silently misbehave once it actually runs
    /// in CI.
    pub(crate) fn unset(key: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key: key.to_owned(), prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}
