use std::time::Duration;

use nosleep::{NoSleep, NoSleepType};
use tracing::{debug, warn};

/// Manages system sleep inhibition to ensure scheduled tasks fire on time.
///
/// When a scheduled task is due within the configured window (default 4 hours),
/// the guard prevents the system from sleeping. When no tasks are imminent,
/// the guard is released.
pub struct SleepGuard {
    /// How far in advance to acquire the sleep guard (in hours).
    window_hours: f64,
    /// Whether the sleep guard is disabled entirely.
    disabled: bool,
    /// Whether an active guard should also keep the display on, rather than
    /// only preventing system/CPU sleep.
    keep_display_awake: bool,
    /// Active nosleep handle, if currently held.
    handle: Option<NoSleep>,
}

impl SleepGuard {
    pub fn new(window_hours: f64) -> Self {
        Self {
            window_hours,
            disabled: false,
            keep_display_awake: false,
            handle: None,
        }
    }

    /// Update the guard based on the nearest upcoming task fire time.
    ///
    /// If a task is due within the configured window, acquire the guard.
    /// Otherwise, release it.
    pub fn update(&mut self, nearest_fire_in: Option<Duration>) {
        let should_hold = self.window_should_hold(nearest_fire_in);

        if should_hold && self.handle.is_none() {
            self.acquire();
        } else if !should_hold && self.handle.is_some() {
            self.release();
        }
    }

    /// Pure decision behind [`update`]: given how long until the nearest
    /// upcoming fire, should the guard be held right now? True only when the
    /// guard is enabled and that fire falls within the configured window.
    /// Split out so the window logic can be unit-tested without acquiring a
    /// real OS-level power assertion.
    fn window_should_hold(&self, nearest_fire_in: Option<Duration>) -> bool {
        if self.disabled {
            return false;
        }
        match nearest_fire_in {
            Some(dur) => dur.as_secs_f64() < self.window_hours * 3600.0,
            None => false,
        }
    }

    /// Update the configured window duration (in hours).
    pub fn set_window_hours(&mut self, hours: f64) {
        self.window_hours = hours;
        self.disabled = false;
    }

    /// Enable or disable the sleep guard entirely.
    pub fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
        if disabled {
            self.release();
        }
    }

    /// Set whether an active guard should also keep the display on. If a
    /// guard is currently held and this actually changes the flag, the
    /// guard is released and immediately re-acquired so the new assertion
    /// type takes effect right away. If no guard is held, the flag is just
    /// stored for the next `acquire()`.
    pub fn set_keep_display_awake(&mut self, on: bool) {
        if self.keep_display_awake == on {
            return;
        }
        self.keep_display_awake = on;
        if self.handle.is_some() {
            self.release();
            self.acquire();
        }
    }

    /// Update the guard based on a boolean "is something active" signal.
    /// Used by callers (e.g. workflow queue manager) that hold the guard for
    /// as long as work is in flight, rather than a time-until-next-fire window.
    pub fn update_active(&mut self, active: bool) {
        let should_hold = active && !self.disabled;
        if should_hold && self.handle.is_none() {
            self.acquire();
        } else if !should_hold && self.handle.is_some() {
            self.release();
        }
    }

    /// The nosleep assertion type to use for the next `acquire()`, based on
    /// the `keep_display_awake` flag.
    fn assertion_type(&self) -> NoSleepType {
        if self.keep_display_awake {
            NoSleepType::PreventUserIdleDisplaySleep
        } else {
            NoSleepType::PreventUserIdleSystemSleep
        }
    }

    fn acquire(&mut self) {
        let assertion_type = self.assertion_type();
        match NoSleep::new() {
            Ok(mut ns) => {
                if let Err(e) = ns.start(assertion_type) {
                    warn!(error = %e, "Failed to start sleep guard");
                    return;
                }
                debug!(
                    window_hours = self.window_hours,
                    keep_display_awake = self.keep_display_awake,
                    "Sleep guard acquired — preventing system sleep"
                );
                self.handle = Some(ns);
            }
            Err(e) => {
                warn!(error = %e, "Failed to create sleep guard");
            }
        }
    }

    fn release(&mut self) {
        if let Some(ref mut ns) = self.handle {
            // nosleep stop is best-effort
            let _ = ns.stop();
        }
        self.handle = None;
        debug!("Sleep guard released");
    }

    /// Read-only accessors for callers' tests to confirm preferences reached
    /// this guard's configuration, without depending on the underlying
    /// `NoSleep` OS assertion actually being acquired (see the module-level
    /// note on why that part isn't asserted on directly).
    #[cfg(test)]
    pub(crate) fn window_hours(&self) -> f64 {
        self.window_hours
    }

    #[cfg(test)]
    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled
    }

    #[cfg(test)]
    pub(crate) fn keep_display_awake(&self) -> bool {
        self.keep_display_awake
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assertion_type_defaults_to_system_sleep_only() {
        let guard = SleepGuard::new(4.0);
        assert_eq!(guard.assertion_type(), NoSleepType::PreventUserIdleSystemSleep);
    }

    #[test]
    fn set_keep_display_awake_switches_assertion_type() {
        let mut guard = SleepGuard::new(4.0);
        guard.set_keep_display_awake(true);
        assert_eq!(guard.assertion_type(), NoSleepType::PreventUserIdleDisplaySleep);

        guard.set_keep_display_awake(false);
        assert_eq!(guard.assertion_type(), NoSleepType::PreventUserIdleSystemSleep);
    }

    #[test]
    fn set_keep_display_awake_is_a_noop_without_a_held_handle() {
        // No guard is acquired here, so toggling the flag must not touch
        // `handle` — it should just be remembered for the next acquire().
        let mut guard = SleepGuard::new(4.0);
        assert!(guard.handle.is_none());
        guard.set_keep_display_awake(true);
        assert!(guard.handle.is_none());
        assert!(guard.keep_display_awake);
    }

    #[test]
    fn set_keep_display_awake_same_value_is_a_noop() {
        let mut guard = SleepGuard::new(4.0);
        guard.set_keep_display_awake(false);
        assert!(!guard.keep_display_awake);
        assert!(guard.handle.is_none());
    }

    // The window-decision tests below exercise the scheduler-facing wiring the
    // Assignment convergence had dropped: whether an upcoming cron fire within
    // `max_sleep_guard_hours` should hold the machine awake. They assert the
    // pure predicate only — actually acquiring the OS assertion is verified
    // manually (see `schedule_runner::tick`).

    #[test]
    fn window_holds_when_nearest_fire_is_within_window() {
        let guard = SleepGuard::new(4.0);
        // Fire is one hour out; the window is four hours → hold.
        assert!(guard.window_should_hold(Some(Duration::from_secs(3600))));
    }

    #[test]
    fn window_does_not_hold_when_nearest_fire_is_beyond_window() {
        let guard = SleepGuard::new(4.0);
        // Fire is five hours out; the window is four hours → do not hold yet.
        assert!(!guard.window_should_hold(Some(Duration::from_secs(5 * 3600))));
    }

    #[test]
    fn window_does_not_hold_when_no_fire_pending() {
        let guard = SleepGuard::new(4.0);
        assert!(!guard.window_should_hold(None));
    }

    #[test]
    fn disabled_guard_never_holds_even_within_window() {
        // A `None` `max_sleep_guard_hours` preference disables the guard; even
        // an imminent fire must not hold the machine awake.
        let mut guard = SleepGuard::new(4.0);
        guard.set_disabled(true);
        assert!(!guard.window_should_hold(Some(Duration::from_secs(60))));
    }
}
