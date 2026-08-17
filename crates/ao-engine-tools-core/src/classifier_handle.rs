use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use ao_protocol::tasklist::TaskAssignment;

/// Outcome from a single classifier call via [`ClassifierHandle::classify`].
///
/// The caller (TodoCreate) maps each variant to the appropriate follow-up
/// action: set assignment on `Assigned`, retry on `Retryable`, leave None on
/// `Permanent` after retry budget exhaustion.
#[derive(Debug, Clone)]
pub enum ClassifyOutcome {
    Assigned(TaskAssignment),
    Retryable(String),
    Permanent(String),
}

/// Abstraction over the task classifier, injected into [`RunnerContext`].
///
/// Defined here (in `ao-engine-tools-core`) so that `ao-engine-tools-engine`
/// tools can call classify without depending on `ao-engine` directly — the
/// concrete implementation lives in `ao-engine::TaskClassifier`.
#[async_trait]
pub trait ClassifierHandle: Send + Sync {
    async fn classify(
        &self,
        parent_agent_id: &str,
        task_id: &str,
        task_title: &str,
        task_description: &str,
    ) -> ClassifyOutcome;
}

/// Shared dedup registry tracking which `(agent, tasklist, task)` triples
/// currently have a classifier attempt in flight.
///
/// Both the periodic reconciler and the event-driven spawn sites (Todo* tools,
/// frontend HTTP routes) consult this set before spawning a new attempt so
/// that a reconciler tick cannot re-spawn a task that the original tool call
/// is already classifying. Process-global — one instance lives on `AppState`
/// and is cloned into every spawn site.
///
/// Implemented over `std::sync::Mutex<HashSet>` rather than an async lock
/// because the critical section is a single insert/remove and never spans an
/// `.await` boundary.
#[derive(Debug, Default)]
pub struct ClassifierInFlight {
    inner: Mutex<HashSet<(String, String, String)>>,
}

impl ClassifierInFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to claim a task slot. Returns `Some(ClassifierClaim)` if the slot
    /// was free; the returned guard removes the entry on drop. Returns `None`
    /// if another spawn already owns the slot — caller should skip silently.
    pub fn claim(
        self: &Arc<Self>,
        agent_id: &str,
        tasklist_id: &str,
        task_id: &str,
    ) -> Option<ClassifierClaim> {
        let key = (
            agent_id.to_string(),
            tasklist_id.to_string(),
            task_id.to_string(),
        );
        let mut set = self
            .inner
            .lock()
            .expect("classifier in-flight mutex poisoned");
        if !set.insert(key.clone()) {
            return None;
        }
        Some(ClassifierClaim {
            set: Arc::clone(self),
            key,
        })
    }

    /// Number of currently in-flight classifier attempts. Mostly useful for
    /// telemetry and test assertions.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("classifier in-flight mutex poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test-visible check whether a specific triple is currently claimed.
    pub fn contains(&self, agent_id: &str, tasklist_id: &str, task_id: &str) -> bool {
        let key = (
            agent_id.to_string(),
            tasklist_id.to_string(),
            task_id.to_string(),
        );
        self.inner
            .lock()
            .expect("classifier in-flight mutex poisoned")
            .contains(&key)
    }
}

/// RAII guard for a claimed classifier slot. Dropping the guard releases the
/// slot back to the registry so the reconciler (or another spawn site) can
/// pick it up on the next tick if the task is still unassigned.
pub struct ClassifierClaim {
    set: Arc<ClassifierInFlight>,
    key: (String, String, String),
}

impl Drop for ClassifierClaim {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.inner.lock() {
            set.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_then_drop_releases_slot() {
        let reg = Arc::new(ClassifierInFlight::new());
        let claim = reg.claim("a", "tl", "t1").expect("first claim succeeds");
        assert!(reg.contains("a", "tl", "t1"));
        assert_eq!(reg.len(), 1);

        // Second claim for the same triple is blocked.
        assert!(reg.claim("a", "tl", "t1").is_none());

        drop(claim);
        assert!(!reg.contains("a", "tl", "t1"));
        assert_eq!(reg.len(), 0);

        // After drop, a fresh claim succeeds.
        let claim2 = reg.claim("a", "tl", "t1").expect("re-claim succeeds");
        assert!(reg.contains("a", "tl", "t1"));
        drop(claim2);
    }

    #[test]
    fn distinct_triples_do_not_collide() {
        let reg = Arc::new(ClassifierInFlight::new());
        let _c1 = reg.claim("a", "tl", "t1").unwrap();
        let _c2 = reg.claim("a", "tl", "t2").unwrap();
        let _c3 = reg.claim("a", "tl2", "t1").unwrap();
        let _c4 = reg.claim("b", "tl", "t1").unwrap();
        assert_eq!(reg.len(), 4);
    }
}
