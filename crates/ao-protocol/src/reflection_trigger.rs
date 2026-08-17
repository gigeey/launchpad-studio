//! Seam for the OBSERVE / reflection pass over per-thread conversation history.
//!
//! A conversation thread here is continuous and append-only — there is no
//! "session end" to hang a review step on. Instead, three independent
//! conditions mark a moment worth reflecting on: the thread's working-window
//! anchor rotates forward (content is about to drop out of the model's view
//! — the sharpest cue), the thread goes idle past its active window, or it
//! is explicitly archived. This module defines the event those conditions
//! emit and the subscriber contract a later reflection pass implements to
//! receive them.
//!
//! This module intentionally does NOT implement the reflection pass. It only
//! names the trigger and provides a subscriber seam plus a no-op default so
//! trigger emission can land ahead of the pass that consumes it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Why a [`ReflectionTrigger`] fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionTriggerReason {
    /// The thread's working-window anchor rotated forward — content that was
    /// previously inside the model's live window just fell outside it. This
    /// is the sharpest cue: it fires at the exact moment older content is
    /// about to be forgotten, rather than after the fact.
    AnchorRotated,
    /// The thread has been idle for at least `active_window_minutes`
    /// (detected the next time the thread is dispatched, since there is no
    /// background timer — see `ao_engine::history::select`).
    IdleTimeout,
    /// The thread was explicitly archived (`Thread::archived_at` set).
    Archived,
}

/// Identifies the thread/transcript a [`ReflectionTrigger`] fired for.
///
/// Deliberately thin: it names *what* happened and *where* to look, not the
/// transcript content itself. A subscriber must read `transcript_path` from
/// disk to see the untrimmed history — this event never carries transcript
/// entries. `transcript_path` is the same value stored on the owning
/// `Thread` row (`ao_protocol::thread::Thread::transcript_path`), which a
/// subscriber can use to resolve the full row (and its
/// `distilled_through_ts` watermark) via `ThreadStore`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectionTrigger {
    pub reason: ReflectionTriggerReason,
    /// Owning agent.
    pub agent_id: String,
    /// Absolute path to the JSONL transcript the trigger applies to.
    pub transcript_path: String,
    pub ts: DateTime<Utc>,
}

/// Subscriber seam for the reflection pass.
///
/// Implementations receive one [`ReflectionTrigger`] per firing. The actual
/// reflection work — reading the untrimmed transcript delta since the
/// thread's `distilled_through_ts` watermark, proposing candidate
/// memories/skills, routing them through the safety gate, and advancing the
/// watermark — is deliberately NOT part of this trait; it is separate,
/// later work.
///
/// Implementations must be `Send + Sync`: triggers fire from request-serving
/// async contexts (history selection, thread archival) and must not block
/// the caller, so a real implementation should hand the trigger off (e.g. to
/// a channel or a spawned task) rather than do the reflection work inline.
pub trait ReflectionTriggerSubscriber: Send + Sync {
    fn on_reflection_trigger(&self, trigger: ReflectionTrigger);
}

/// Default subscriber that does nothing. Wired in everywhere a real
/// subscriber isn't supplied yet, so trigger emission is safe to land ahead
/// of the pass that will eventually consume it.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopReflectionSubscriber;

impl ReflectionTriggerSubscriber for NoopReflectionSubscriber {
    // TODO(reflection): implement the real subscriber — read the
    // untrimmed transcript delta since the thread's `distilled_through_ts`
    // watermark (ao-persistence/src/thread_store.rs), propose candidate
    // memories/skills, route them through the safety gate, and advance
    // the watermark.
    fn on_reflection_trigger(&self, _trigger: ReflectionTrigger) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn noop_subscriber_accepts_any_trigger_without_panic() {
        let subscriber = NoopReflectionSubscriber;
        subscriber.on_reflection_trigger(ReflectionTrigger {
            reason: ReflectionTriggerReason::AnchorRotated,
            agent_id: "agent-1".to_string(),
            transcript_path: "/tmp/agent-1.jsonl".to_string(),
            ts: Utc::now(),
        });
    }

    /// A recording stub proves the trait is object-safe (`Arc<dyn ...>`) and
    /// that a real implementation can capture what it's told without any
    /// special-casing by callers.
    struct RecordingSubscriber {
        seen: Arc<Mutex<Vec<ReflectionTrigger>>>,
    }

    impl ReflectionTriggerSubscriber for RecordingSubscriber {
        fn on_reflection_trigger(&self, trigger: ReflectionTrigger) {
            self.seen.lock().unwrap().push(trigger);
        }
    }

    #[test]
    fn subscriber_trait_is_object_safe_and_dispatches() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber: Arc<dyn ReflectionTriggerSubscriber> = Arc::new(RecordingSubscriber {
            seen: Arc::clone(&seen),
        });

        subscriber.on_reflection_trigger(ReflectionTrigger {
            reason: ReflectionTriggerReason::Archived,
            agent_id: "agent-2".to_string(),
            transcript_path: "/tmp/agent-2.jsonl".to_string(),
            ts: Utc::now(),
        });

        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].reason, ReflectionTriggerReason::Archived);
        assert_eq!(recorded[0].agent_id, "agent-2");
    }

    #[test]
    fn reflection_trigger_json_round_trip() {
        let trigger = ReflectionTrigger {
            reason: ReflectionTriggerReason::IdleTimeout,
            agent_id: "agent-3".to_string(),
            transcript_path: "/tmp/agent-3.jsonl".to_string(),
            ts: Utc::now(),
        };
        let json = serde_json::to_string(&trigger).expect("serialize");
        assert!(json.contains("\"idle_timeout\""));
        let round: ReflectionTrigger = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round, trigger);
    }
}
