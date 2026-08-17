use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::{AgentId, ChannelKind};
use crate::assignment::AssignmentId;
use crate::delegation::DelegationId;
use crate::team::TeamId;

pub type ThreadId = String;

/// How a [`Thread`] came to exist.
///
/// `Default` rows are materialized once per agent by the persistence layer
/// (see `ThreadStore::ensure_default_thread`) and alias the agent's
/// pre-thread transcript file in place — no message movement, no copy.
/// `Fresh` and `Branch` rows are operator-created via the HTTP routes and
/// own their own transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadKind {
    #[default]
    Default,
    Fresh,
    Branch,
}

/// Source-of-truth for a `Branch` thread's inherited history.
///
/// `branch_at` is mirrored into [`Thread::history_floor_ts`] when the row is
/// created so a single field carries the window floor at compose time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchSource {
    pub source_thread_id: ThreadId,
    pub branch_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_id: Option<String>,
}

/// Which channel binding created this thread as its dedicated bridge
/// conversation, stamped once at creation time and never mutated afterward.
///
/// Named distinctly from `ao_engine::channels::discord::ChannelOrigin` (an
/// unrelated, crate-private outbound-relay reply-target correlation type) —
/// same two words, different concept: this one answers "why does this
/// thread exist," that one answers "where does a reply to this thread go."
///
/// This is the source of truth for "is this thread a channel bridge thread,
/// and which channel" — it works uniformly whether the channel provisions
/// one thread per binding (Telegram/Discord/Email, whose
/// `ChannelBinding::bridge_thread_id` also names this same thread) or one
/// thread per conversation (Slack, whose `bridge_thread_id` is never
/// populated at runtime — see `resolve_bridge_thread` in
/// `ao_engine::channels::slack::runner`, which mints a fresh thread per
/// `(team_id, channel, thread_ts)` and never writes it back onto the
/// binding). Callers still need to check the named binding's current
/// `enabled` flag live (via `binding_id`) rather than trusting presence of
/// this field alone — disabling a channel un-gates its bridge thread
/// without touching any `Thread` row.
///
/// `None` for every ordinary (non-channel) thread, and for a bridge thread
/// created before this field existed until a backfill pass stamps it (see
/// the startup migration that reads `SlackConversationRegistryStore`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelBridgeOrigin {
    pub kind: ChannelKind,
    /// The owning `ChannelBinding::binding_id`. Stable across disable/
    /// re-enable — `binding_id` is deterministic and never regenerated.
    pub binding_id: String,
}

/// Which assignment created this thread as a run's own conversation,
/// stamped once at thread-creation time and never mutated afterward.
///
/// Set only for the two `AssignmentThreadPolicy` variants under which a
/// thread is genuinely *owned* by one assignment — `Fresh` (a brand-new
/// thread per fire) and `Dedicated` (one thread reused across every fire).
/// Deliberately never set for `Main`, whose runs land in the agent's
/// ordinary default thread alongside interactive chat: stamping that thread
/// would misclassify normal conversation as an assignment run.
///
/// `run_id` distinguishes the two owning policies without a separate field:
/// `Fresh` sets it to the one run that created the thread (a 1:1
/// thread-to-run relationship), while `Dedicated` leaves it `None` since the
/// same thread persists across many runs and no single one owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentBridgeOrigin {
    pub assignment_id: AssignmentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub title: Option<String>,
    /// System-derived label shown while `title` is unset — a trimmed slice of
    /// the thread's first user message (see [`derive_auto_title`]). Kept
    /// separate from `title` so it never counts as "explicitly named": the
    /// `RenameThread` tool and the human rename route both gate on
    /// `title.is_none()`, not on this field. Cleared back to `None` only by
    /// convention, never automatically — once `title` is set, `auto_title` is
    /// simply ignored by every fallback-chain consumer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_title: Option<String>,
    pub scope: ThreadScope,
    /// Absolute path to the JSONL transcript backing this thread. For
    /// `Default` threads this equals the agent's pre-existing
    /// `{root}/messages/data/{agent_id}.jsonl`, which is reused in place so
    /// the migration is provably non-destructive.
    pub transcript_path: String,
    #[serde(default)]
    pub kind: ThreadKind,
    /// When set, history-window selection treats messages with
    /// `ts < history_floor_ts` as outside the live window for this thread.
    /// Identical in semantics to `RunnerContext::window_floor_ts`. Populated
    /// for `Branch` threads with the same value as `branch_source.branch_at`;
    /// left `None` for `Default` and `Fresh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_floor_ts: Option<DateTime<Utc>>,
    /// Distillation watermark — "reflected up to
    /// here". A later reflection pass reads only the untrimmed transcript
    /// delta with `ts > distilled_through_ts` and advances this field after
    /// staging what it found, making each pass idempotent — a rotation that
    /// finds nothing new since the last pass is a cheap no-op instead of a
    /// full re-read. `None` means never distilled (including every
    /// pre-existing row from before this field existed, via `serde(default)`).
    ///
    /// Mirrors `history_floor_ts`'s storage shape exactly, but the two are
    /// independent: `history_floor_ts` is a Branch's fixed fork point (set
    /// once, never advances); `distilled_through_ts` moves forward over the
    /// life of any thread. For a Branch specifically, starting this at `None`
    /// (rather than inheriting the source's watermark) is what keeps
    /// branches safe — a Branch only ever distills its own post-fork delta,
    /// since the shared prefix was already handled on the source thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distilled_through_ts: Option<DateTime<Utc>>,
    /// Periodic in-life promotion-sweep watermark: "last time this thread's
    /// accumulated thread-scope memory was checked against the promotion
    /// judge outside of archival" (see
    /// `ao_engine::reflection_subscriber::ReflectionSubscriber::run_periodic_promotion_sweep`).
    /// `None` means never swept, including every pre-existing row from
    /// before this field existed (via `serde(default)`).
    ///
    /// Distinct from `distilled_through_ts`: that watermark tracks how much
    /// of the raw transcript has been distilled into candidate proposals;
    /// this one tracks how recently the thread-scope memory ENTRIES those
    /// proposals produced were last checked for promotion into durable
    /// scope. The two advance independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_swept_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_source: Option<BranchSource>,
    /// Set when the thread has been archived — hidden from the tab strip,
    /// the overflow panel, `ThreadsPanel`'s main list, and Home's per-agent
    /// thread list, without touching the transcript or metadata otherwise.
    /// `None` means visible everywhere (the default for every thread).
    /// Cleared back to `None` by unarchiving, which is the only way to bring
    /// a thread back into those surfaces. Never set for a `Default` thread —
    /// see [`ThreadStore::archive`], which refuses the same way
    /// [`ThreadStore::delete`] refuses to delete one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
    /// Set when this thread was created as a channel binding's dedicated
    /// bridge conversation. See [`ChannelBridgeOrigin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_origin: Option<ChannelBridgeOrigin>,
    /// Set when this thread was created as an assignment's own run
    /// conversation. See [`AssignmentBridgeOrigin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_origin: Option<AssignmentBridgeOrigin>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ThreadScope {
    AgentChat { agent_id: AgentId },
    TeamChat { team_id: TeamId },
    Delegation { team_id: TeamId, delegation_id: DelegationId },
    /// One artifact's chat mini-thread. Never surfaced by
    /// `ThreadStore::list_for_agent` (that filter only matches `AgentChat`) —
    /// this scope exists so the artifact chat panel's transcript is a regular
    /// [`Thread`] row instead of raw path I/O, not so it gains a tab-strip
    /// presence.
    Artifact { artifact_id: String },
}

/// Deterministic id for an agent's `Default` thread.
///
/// Any layer that needs to resolve "the agent's history" without a
/// `thread_id` parameter can compute this directly and skip a store lookup.
/// Mirrored by `ThreadStore::default_thread_id`.
pub fn default_thread_id(agent_id: &str) -> ThreadId {
    format!("default-{agent_id}")
}

/// Deterministic id for an artifact's chat mini-thread.
///
/// Keyed solely by `artifact_id` (no `agent_id`) since artifact ids are
/// already globally unique. Mirrored by `ThreadStore::artifact_thread_id`.
pub fn artifact_thread_id(artifact_id: &str) -> ThreadId {
    format!("artifact-{artifact_id}")
}

/// Longest `auto_title` / tool-supplied `title` kept server-side. Generous on
/// purpose — this is the value a tooltip shows in full; UI surfaces that only
/// have room for a short label (e.g. a tab strip) truncate further on their
/// own, client-side.
pub const MAX_TITLE_LEN: usize = 48;

impl Thread {
    /// Whether the `RenameThread` tool should be offered to the model for a
    /// run scoped to this thread.
    ///
    /// `true` only for a personal, non-default thread that has never been
    /// explicitly named (`title.is_none()`) — the tool's whole purpose. The
    /// agent's `Default` thread has a hardcoded tab label regardless of
    /// `title`, so naming it would be a silent no-op; `TeamChat` and
    /// `Delegation` threads are shared/ephemeral surfaces that don't carry a
    /// user-facing tab identity the way a personal chat thread does. Callers
    /// use this to decide whether to register the tool for a run at all
    /// (rather than always registering it and relying solely on the
    /// in-tool refusal) so a thread that's already named never pays the
    /// token/latency cost of an irrelevant tool definition.
    pub fn offers_rename_tool(&self) -> bool {
        self.title.is_none()
            && self.kind != ThreadKind::Default
            && matches!(self.scope, ThreadScope::AgentChat { .. })
    }

    /// Whether this thread is currently archived — hidden from the surfaces
    /// listed on [`Self::archived_at`] until unarchived.
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// Derive a short auto-title from a thread's first user message: collapse all
/// whitespace runs (including newlines) to single spaces, trim the ends, and
/// truncate to [`MAX_TITLE_LEN`] chars (char-boundary safe, with a trailing
/// `…` when truncated). Returns `None` for input that's empty after
/// trimming/collapsing so callers never persist a blank auto-title.
pub fn derive_auto_title(content: &str) -> Option<String> {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let char_count = collapsed.chars().count();
    if char_count <= MAX_TITLE_LEN {
        return Some(collapsed);
    }
    let truncated: String = collapsed.chars().take(MAX_TITLE_LEN).collect();
    Some(format!("{truncated}…"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_internal_whitespace_and_newlines() {
        assert_eq!(
            derive_auto_title("hello\n\n  world   foo"),
            Some("hello world foo".to_string())
        );
    }

    #[test]
    fn trims_leading_and_trailing_whitespace() {
        assert_eq!(
            derive_auto_title("   spaced out   "),
            Some("spaced out".to_string())
        );
    }

    #[test]
    fn blank_input_returns_none() {
        assert_eq!(derive_auto_title(""), None);
        assert_eq!(derive_auto_title("   \n\t  "), None);
    }

    #[test]
    fn truncates_long_content_with_ellipsis() {
        let long = "a".repeat(100);
        let result = derive_auto_title(&long).unwrap();
        assert_eq!(result.chars().count(), MAX_TITLE_LEN + 1); // +1 for the ellipsis char
        assert!(result.ends_with('…'));
    }

    #[test]
    fn short_content_is_untouched_and_unellipsized() {
        let result = derive_auto_title("fix the login bug").unwrap();
        assert_eq!(result, "fix the login bug");
        assert!(!result.ends_with('…'));
    }

    #[test]
    fn truncation_is_char_boundary_safe_for_multibyte_content() {
        // Every char here is a multi-byte emoji; a byte-index truncation
        // would panic or slice mid-codepoint.
        let long = "😀".repeat(60);
        let result = derive_auto_title(&long).unwrap();
        assert_eq!(result.chars().count(), MAX_TITLE_LEN + 1);
    }

    #[test]
    fn offers_rename_tool_true_only_for_unnamed_personal_non_default_thread() {
        let base = Thread {
            id: "t1".to_string(),
            title: None,
            auto_title: None,
            scope: ThreadScope::AgentChat { agent_id: "a1".to_string() },
            transcript_path: "/tmp/x".to_string(),
            kind: ThreadKind::Fresh,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(base.offers_rename_tool());

        let mut named = base.clone();
        named.title = Some("Named".to_string());
        assert!(!named.offers_rename_tool());

        let mut default_kind = base.clone();
        default_kind.kind = ThreadKind::Default;
        assert!(!default_kind.offers_rename_tool());

        let mut team = base.clone();
        team.scope = ThreadScope::TeamChat { team_id: "team1".to_string() };
        assert!(!team.offers_rename_tool());

        let mut delegation = base.clone();
        delegation.scope = ThreadScope::Delegation {
            team_id: "team1".to_string(),
            delegation_id: "d1".to_string(),
        };
        assert!(!delegation.offers_rename_tool());

        // auto_title being set does NOT block eligibility — only an explicit
        // `title` does.
        let mut auto_named = base.clone();
        auto_named.auto_title = Some("Auto".to_string());
        assert!(auto_named.offers_rename_tool());
    }

    #[test]
    fn is_archived_reflects_archived_at() {
        let mut thread = Thread {
            id: "t1".to_string(),
            title: None,
            auto_title: None,
            scope: ThreadScope::AgentChat { agent_id: "a1".to_string() },
            transcript_path: "/tmp/x".to_string(),
            kind: ThreadKind::Fresh,
            history_floor_ts: None,
            distilled_through_ts: None,
            promotion_swept_at: None,
            branch_source: None,
            archived_at: None,
            channel_origin: None,
            assignment_origin: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(!thread.is_archived());

        thread.archived_at = Some(Utc::now());
        assert!(thread.is_archived());
    }
}
