use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which renderer draws an artifact's payload. Orthogonal to [`RefreshIntent`]
/// (whether/how the artifact updates) — a kind is set independently of its
/// liveness.
///
/// `Unknown` is the forward-compat escape hatch: artifact storage carries no
/// per-row schema version, so an app build that adds a new kind must not make
/// an older build's read of that row fail outright. Any wire string this
/// build doesn't recognize deserializes to `Unknown` instead of erroring, and
/// the renderer treats `Unknown` as an inert "unsupported artifact type"
/// fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    List,
    Cards,
    Table,
    Board,
    Metric,
    Chart,
    Html,
    #[serde(other)]
    Unknown,
}

/// How an artifact's blob is encoded on disk, independent of [`ArtifactKind`]
/// (which renderer draws it).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PayloadFormat {
    Json,
    Html,
}

/// Whether and how an artifact updates after it is first rendered.
///
/// `WholeArtifact` rides the existing background-agent loop: the host replays
/// [`ArtifactRecord::origin_intent`] and the agent re-emits a fresh payload.
/// `Brokered` is reserved for the planned in-artifact bridge (granular,
/// per-slice callbacks) — the variant exists so the storage shape never needs
/// a migration when that capability ships, but no v1 write path emits it.
///
/// Carries the same `Unknown` forward-compat fallback as [`ArtifactKind`]: an
/// unrecognized wire string deserializes to `Unknown` rather than failing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefreshIntent {
    None,
    WholeArtifact,
    Brokered,
    #[serde(other)]
    Unknown,
}

impl Default for RefreshIntent {
    fn default() -> Self {
        RefreshIntent::None
    }
}

/// One allowlisted data slice an artifact may request from the planned
/// in-artifact bridge. Declared at author time; the host serves only slices
/// on this list, so a sandboxed artifact can never trigger arbitrary
/// tool/agent invocation. Stored now (so the record shape is stable ahead of
/// need) but not served until the bridge ships — see [`RefreshIntent::Brokered`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilitySpec {
    pub slice: String,
    pub params_schema: Value,
}

/// The replayable authoring request that produced an artifact — the host
/// replays this to regenerate the payload on a whole-artifact refresh
/// (`RefreshIntent::WholeArtifact`). The full shape is owned by the
/// `ArtifactWrite` authoring tool; a self-contained refresh instruction is
/// enough for this storage layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OriginIntent {
    pub refresh_prompt: String,
}

/// Which surface performed a mutation recorded in an artifact's
/// [`ArtifactRecord::intent_ledger`]. Carries the same `Unknown` forward-compat
/// fallback as [`ArtifactKind`]/[`RefreshIntent`] — an older build must not
/// fail to deserialize a ledger entry tagged by a newer build's surface.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentSource {
    /// The artifact's initial `ArtifactStore::create` call.
    Create,
    /// An `ArtifactWrite(id=...)` call made by the background subagent
    /// spawned off `POST .../regenerate` (whole-artifact replay).
    Regenerate,
    /// A direct in-place payload replacement issued outside the model loop
    /// (e.g. `PUT .../refresh`), bypassing `ArtifactWrite`.
    MainThreadEdit,
    /// An `ArtifactWrite(id=...)` edit-in-place issued by the model during a
    /// normal conversational turn.
    Chat,
    /// `POST .../undo` restored a prior body off [`ArtifactRecord::history`].
    /// The one mutation source whose write never itself pushes a new history
    /// snapshot — doing so would archive the very body the undo is
    /// discarding and corrupt the snapshot stack.
    Undo,
    #[serde(other)]
    Unknown,
}

/// Upper bound on [`ArtifactRecord::intent_ledger`]'s length. Once a mutation
/// pushes the ledger past this many entries, the oldest evictable entry is
/// dropped — see that field's doc comment for why the creation entry (index
/// 0) is exempt.
pub const INTENT_LEDGER_MAX_LEN: usize = 20;

/// One append-only record of who touched an artifact and why, written once
/// per mutation at the `ArtifactStore::create`/`ArtifactStore::refresh`
/// choke point in `ao_persistence::artifact_store` — see
/// [`ArtifactRecord::intent_ledger`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntentLedgerEntry {
    pub timestamp: DateTime<Utc>,
    pub source: IntentSource,
    /// Point-in-time, artifact-scoped summary of what this particular
    /// mutation was for, as supplied by the caller. `None` when the caller
    /// didn't supply one — e.g. an `ArtifactWrite` call that omitted
    /// `intent_note`, or a direct API edit with no note to give.
    #[serde(default)]
    pub intent_note: Option<String>,
    /// Id of the message that triggered this mutation, when known. Mirrors
    /// [`ArtifactRecord::source_message_id`]'s provenance rule (stamped from
    /// the producing turn's context, never taken from model input) but
    /// per-entry rather than creation-only.
    #[serde(default)]
    pub source_message_id: Option<String>,
}

/// Upper bound on [`ArtifactRecord::history`]'s length. Once a body-replacing
/// mutation pushes the snapshot stack past this many entries, the oldest
/// snapshot is evicted (both its registry entry and its on-disk blob) — see
/// that field's doc comment. Unlike [`INTENT_LEDGER_MAX_LEN`]'s creation-entry
/// exemption, every entry here is equally evictable: `history` never holds
/// the artifact's very first body, only prior bodies superseded by a later
/// edit.
pub const ARTIFACT_HISTORY_MAX_LEN: usize = 10;

/// One prior body an artifact used to have, kept around so [`IntentSource::Undo`]
/// has something to restore. Pushed onto [`ArtifactRecord::history`] at the
/// `ArtifactStore::refresh` choke point immediately before that call
/// overwrites the current body — see that field's doc comment for the
/// snapshot-on-edit design this supports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactHistoryEntry {
    /// Monotonically increasing per-artifact sequence number, stamped from
    /// [`ArtifactRecord::next_history_seq`] at push time. Stays stable across
    /// eviction (unlike a plain index into `history`, which shifts every
    /// time the oldest entry drops off), so a snapshot keeps the same
    /// identity for as long as it's kept.
    pub seq: u64,
    pub checksum_sha256: String,
    pub size_bytes: u64,
    /// When this body was superseded (i.e. when the *next* edit landed), not
    /// when the body itself was originally written.
    pub timestamp: DateTime<Utc>,
    /// Provenance of the body this snapshot holds — the [`IntentSource`] that
    /// was current on the artifact's most recent intent-ledger entry at the
    /// moment this snapshot was taken.
    pub source: IntentSource,
    /// Blob filename under the store's per-artifact history directory
    /// (sibling of the main payload blob directory, not inside it) — see
    /// `ao_persistence::paths::DataRoot::artifact_history_dir`.
    pub stored_filename: String,
}

/// A renderable output an agent produced — a typed dataset (list/cards/table/
/// board/metric/chart) or freeform HTML — persisted so it can be displayed,
/// refreshed in place, and reopened later.
///
/// The payload itself lives as a file on disk (`stored_filename`, under the
/// store's per-agent blob directory); this record is the pointer plus
/// metadata, mirroring how attachment records point at uploaded files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactRecord {
    /// Unprefixed `uuid::Uuid::new_v4()` string.
    pub id: String,
    /// Card and window title.
    pub title: String,
    pub kind: ArtifactKind,
    /// How to read the blob at `stored_filename` (JSON vs. HTML), independent
    /// of `kind` (which renderer draws it).
    pub format: PayloadFormat,
    /// Blob filename under the store's per-agent blob directory.
    pub stored_filename: String,
    pub size_bytes: u64,
    /// Change-detection hash of the current payload, recomputed on every
    /// refresh.
    pub checksum_sha256: String,

    pub refresh_intent: RefreshIntent,
    /// The request that produced this artifact, replayed to regenerate the
    /// payload on refresh. `None` means the artifact cannot be refreshed
    /// (must pair with `refresh_intent: None`).
    #[serde(default)]
    pub origin_intent: Option<OriginIntent>,

    /// Tier-3 fetch allowlist declared at author time. Reserved for the
    /// planned in-artifact bridge; stored now, served nowhere yet.
    #[serde(default)]
    pub capabilities: Vec<CapabilitySpec>,

    /// Id of the message that produced this artifact, enabling inline
    /// resolution in a thread bubble.
    #[serde(default)]
    pub source_message_id: Option<String>,

    pub created_at: DateTime<Utc>,
    /// Bumped on every refresh — the payload is mutated in place, so this is
    /// the field callers check to know whether a re-render is warranted.
    #[serde(default)]
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_refreshed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub refresh_count: u32,

    /// Set by the Assets page's pin action: exempts the artifact from being
    /// just another entry in its owning agent's list and surfaces it on the
    /// cross-agent pinned view. Purely a display flag — pinning never affects
    /// eviction, since artifacts have no eviction sweep today (unlike the
    /// memory review queue's `pinned`, which this mirrors in spirit).
    #[serde(default)]
    pub pinned: bool,

    /// When this artifact was last pinned (`None` if never pinned or
    /// currently unpinned). Distinct from `created_at` on purpose: the
    /// Assets sidebar orders pinned artifacts newest-pinned-first, and an
    /// artifact created long ago but pinned today should rise to the top,
    /// not sort by its original creation date.
    #[serde(default)]
    pub pinned_at: Option<DateTime<Utc>>,

    /// Id of the [`ArtifactGroup`] this artifact is filed under in the
    /// Assets sidebar, or `None` for the default ungrouped list. Only
    /// meaningful while `pinned` — groups exist to organize the cross-agent
    /// pinned view.
    #[serde(default)]
    pub group_id: Option<String>,

    /// Append-only, bounded history of why this artifact was written or
    /// rewritten — one [`IntentLedgerEntry`] per `create`/`refresh` call,
    /// oldest (the creation entry) first. Maintained entirely by the
    /// artifact store — no mutating surface needs its own bookkeeping to
    /// keep it up to date. `#[serde(default)]` so artifacts persisted before
    /// this field existed still deserialize, with an empty ledger.
    #[serde(default)]
    pub intent_ledger: Vec<IntentLedgerEntry>,

    /// Bounded, oldest-first stack of prior bodies this artifact used to
    /// have, capped at [`ARTIFACT_HISTORY_MAX_LEN`] (oldest evicted first).
    /// Every body-replacing `ArtifactStore::refresh` call pushes the body it
    /// is about to overwrite onto this stack before writing — CREATE has no
    /// prior body, so it starts empty. `POST .../undo` pops the last entry
    /// and restores it as the current body, which is the entire undo
    /// mechanism: no redo, no browsable history, just "go back one (or a
    /// few) edits." `#[serde(default)]` so artifacts persisted before this
    /// field existed still deserialize, with an empty (i.e. no-undo-
    /// available) history.
    #[serde(default)]
    pub history: Vec<ArtifactHistoryEntry>,

    /// Monotonically increasing counter used to stamp the next pushed
    /// [`ArtifactHistoryEntry::seq`]. Lives on the record (not derived from
    /// `history.len()`) so sequence numbers stay unique and increasing even
    /// as old entries are evicted off the bounded stack.
    #[serde(default)]
    pub next_history_seq: u64,
}

/// A user-defined bucket for organizing pinned artifacts in the Assets
/// sidebar (e.g. "Q3 launch", "research"), rendered as a collapsible section
/// above the ungrouped list. Global rather than per-agent, matching the
/// cross-agent scope of the pinned view itself — see
/// [`ArtifactRecord::group_id`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactGroup {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> ArtifactRecord {
        let now = Utc::now();
        ArtifactRecord {
            id: uuid::Uuid::new_v4().to_string(),
            title: "Inbox highlights".to_string(),
            kind: ArtifactKind::Cards,
            format: PayloadFormat::Json,
            stored_filename: "abc123.json".to_string(),
            size_bytes: 42,
            checksum_sha256: "deadbeef".to_string(),
            refresh_intent: RefreshIntent::WholeArtifact,
            origin_intent: Some(OriginIntent {
                refresh_prompt: "Summarize today's unread emails as cards.".to_string(),
            }),
            capabilities: vec![CapabilitySpec {
                slice: "email.body".to_string(),
                params_schema: serde_json::json!({ "type": "object" }),
            }],
            source_message_id: Some("msg-1".to_string()),
            created_at: now,
            updated_at: now,
            last_refreshed_at: None,
            refresh_count: 0,
            pinned: false,
            pinned_at: None,
            group_id: None,
            intent_ledger: vec![IntentLedgerEntry {
                timestamp: now,
                source: IntentSource::Create,
                intent_note: Some("Summarize today's unread emails as cards.".to_string()),
                source_message_id: Some("msg-1".to_string()),
            }],
            history: vec![ArtifactHistoryEntry {
                seq: 0,
                checksum_sha256: "beefdead".to_string(),
                size_bytes: 40,
                timestamp: now,
                source: IntentSource::Chat,
                stored_filename: "abc123-0.json".to_string(),
            }],
            next_history_seq: 1,
        }
    }

    #[test]
    fn artifact_record_json_round_trip() {
        let record = sample_record();
        let json = serde_json::to_string(&record).expect("serialize");
        let deserialized: ArtifactRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record, deserialized);
    }

    #[test]
    fn unknown_kind_and_refresh_intent_strings_deserialize_to_catch_all() {
        // Simulates a row written by a newer app build that introduced a
        // kind/refresh_intent variant this build doesn't know about yet. The
        // read must degrade to the catch-all, never fail outright — there is
        // no per-row version field to gate on.
        let json = r#"{
            "id": "artifact-1",
            "title": "Live dashboard",
            "kind": "timeline",
            "format": "html",
            "stored_filename": "artifact-1.html",
            "size_bytes": 10,
            "checksum_sha256": "abc",
            "refresh_intent": "granular_slice",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let record: ArtifactRecord = serde_json::from_str(json).expect("legacy-tolerant read");
        assert_eq!(record.kind, ArtifactKind::Unknown);
        assert_eq!(record.refresh_intent, RefreshIntent::Unknown);
        // Fields added after the row was first written come back via defaults.
        assert!(record.origin_intent.is_none());
        assert!(record.capabilities.is_empty());
        assert!(record.source_message_id.is_none());
        assert!(record.last_refreshed_at.is_none());
        assert_eq!(record.refresh_count, 0);
        assert!(!record.pinned);
        assert!(record.pinned_at.is_none());
        assert!(record.group_id.is_none());
        assert!(record.intent_ledger.is_empty());
        assert!(record.history.is_empty());
        assert_eq!(record.next_history_seq, 0);
    }

    #[test]
    fn missing_additive_fields_deserialize_via_serde_default() {
        // An even older row than the one above: predates `capabilities` and
        // `refresh_count` entirely (not merely an unrecognized value — the
        // keys are absent). Must not fail to deserialize.
        let json = r#"{
            "id": "artifact-2",
            "title": "Weekly metrics",
            "kind": "metric",
            "format": "json",
            "stored_filename": "artifact-2.json",
            "size_bytes": 4,
            "checksum_sha256": "abc",
            "refresh_intent": "none",
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let record: ArtifactRecord = serde_json::from_str(json).expect("back-compat read");
        assert!(record.capabilities.is_empty());
        assert_eq!(record.refresh_count, 0);
        assert!(record.origin_intent.is_none());
        assert!(record.source_message_id.is_none());
        assert!(record.last_refreshed_at.is_none());
        assert!(!record.pinned);
        assert!(record.pinned_at.is_none());
        assert!(record.group_id.is_none());
        assert!(record.intent_ledger.is_empty());
        assert!(record.history.is_empty());
        assert_eq!(record.next_history_seq, 0);
    }

    #[test]
    fn known_kind_strings_use_snake_case_wire_form() {
        for (kind, wire) in [
            (ArtifactKind::List, "\"list\""),
            (ArtifactKind::Cards, "\"cards\""),
            (ArtifactKind::Table, "\"table\""),
            (ArtifactKind::Board, "\"board\""),
            (ArtifactKind::Metric, "\"metric\""),
            (ArtifactKind::Chart, "\"chart\""),
            (ArtifactKind::Html, "\"html\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
        }
    }

    #[test]
    fn refresh_intent_default_is_none() {
        assert_eq!(RefreshIntent::default(), RefreshIntent::None);
    }

    #[test]
    fn undo_intent_source_uses_snake_case_wire_form() {
        assert_eq!(serde_json::to_string(&IntentSource::Undo).unwrap(), "\"undo\"");
    }

    #[test]
    fn artifact_history_entry_json_round_trip() {
        let entry = ArtifactHistoryEntry {
            seq: 3,
            checksum_sha256: "abc123".to_string(),
            size_bytes: 17,
            timestamp: Utc::now(),
            source: IntentSource::Chat,
            stored_filename: "artifact-1-3.json".to_string(),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let deserialized: ArtifactHistoryEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, deserialized);
    }
}
