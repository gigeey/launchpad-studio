use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Process-global count of in-flight background/delegate agent work.
///
/// Lives here (rather than in `ao-engine` or `ao-engine-tools-core`) because
/// it needs to be reachable from both the spawn side — the background-agent
/// runner in `ao-engine-tools-runner`, which deliberately has no dependency
/// on `ao-engine` — and the read side — `ao-engine`'s sleep-guard poller.
/// `ao-protocol` is the lowest crate both already depend on.
fn counter() -> &'static AtomicUsize {
    static COUNTER: OnceLock<AtomicUsize> = OnceLock::new();
    COUNTER.get_or_init(|| AtomicUsize::new(0))
}

/// RAII handle representing one in-flight piece of background agent work.
///
/// Decrements the global counter on drop, including on panic, so a crashed
/// background task can never leave the counter stuck above zero.
pub struct BackgroundActivityGuard {
    _private: (),
}

impl Drop for BackgroundActivityGuard {
    fn drop(&mut self) {
        counter().fetch_sub(1, Ordering::AcqRel);
    }
}

/// Mark one unit of background agent work as started. Hold the returned
/// guard for the lifetime of that work; dropping it (including via an
/// unwinding panic) marks it as finished.
pub fn background_activity_guard() -> BackgroundActivityGuard {
    counter().fetch_add(1, Ordering::AcqRel);
    BackgroundActivityGuard { _private: () }
}

/// Current count of in-flight background agent work, process-wide.
pub fn background_activity_count() -> usize {
    counter().load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The counter above is a single process-global static, so tests that
    // observe it via before/after deltas must not run concurrently with each
    // other (they'd race on the shared count). Serialize just these tests
    // rather than pulling in a `#[serial]` crate for one module. `catch_unwind`
    // below stops the panic before it can unwind through this lock guard, so
    // the lock never gets poisoned.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn guard_increments_on_construct_and_decrements_on_drop() {
        let _serial = TEST_LOCK.lock().unwrap();
        let before = background_activity_count();

        let guard = background_activity_guard();
        assert_eq!(background_activity_count(), before + 1);

        drop(guard);
        assert_eq!(background_activity_count(), before);
    }

    #[test]
    fn count_returns_to_baseline_after_drop_on_panic() {
        let _serial = TEST_LOCK.lock().unwrap();
        let before = background_activity_count();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = background_activity_guard();
            assert_eq!(background_activity_count(), before + 1);
            panic!("simulated background task panic");
        }));

        assert!(result.is_err(), "the panic should have propagated out of catch_unwind");
        assert_eq!(
            background_activity_count(),
            before,
            "guard must release the count even when its task panics"
        );
    }

    #[test]
    fn multiple_guards_stack_and_unwind_in_any_order() {
        let _serial = TEST_LOCK.lock().unwrap();
        let before = background_activity_count();

        let a = background_activity_guard();
        let b = background_activity_guard();
        assert_eq!(background_activity_count(), before + 2);

        drop(a);
        assert_eq!(background_activity_count(), before + 1);

        drop(b);
        assert_eq!(background_activity_count(), before);
    }
}
