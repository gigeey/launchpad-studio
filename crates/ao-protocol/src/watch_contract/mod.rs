//! Watch Contracts — deterministic dedup for `AssignmentTrigger::AgentWatch`.
//!
//! An agent-driven watch used to decide, on its own, what counted as "the
//! same item" on every single poll — the model synthesized an id string each
//! time, and the vocabulary it picked drifted poll to poll, so exact-match
//! dedup saw a stream of "new" items that were really the same one. A
//! [`WatchContract`] moves that decision out of the hot path: it is authored
//! once, on the watch's first run, and every subsequent poll is bound by it —
//! the model fills in the declared fields, and code (this module) computes
//! the identity/version/predicate keys and decides fire-or-skip.
//!
//! `identity.strategy` is a fallback ladder ([`IdentityStrategy`]), not a
//! special case for any one connector — no source system is named anywhere
//! in this module, in a comment, or in an error string. The whole point of
//! the ladder is that a source nobody anticipated still works on day one: the
//! agent discovers whatever field that source uses as a stable key (or
//! decides no such field exists) and records the choice in the contract,
//! not in a match arm here.
//!
//! Everything in this module is pure: no I/O, no async, no clock reads. The
//! runtime tick (a later piece of this feature) is the only caller that
//! touches persistence or fires anything.

use std::collections::HashMap;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract_primitives::{
    canonical_json, normalize_identity_text, normalize_value_for_hash, normalize_value_for_identity, sha256_hex,
};
use crate::predicate::{evaluate_predicate as evaluate_typed_predicate, Predicate};

pub mod legacy_expr;

/// A watch's frozen declaration of what it observes and what identity,
/// change, and firing mean for it. Authored once by the watch's own agent on
/// its first run, then injected unmodified into every subsequent poll — see
/// the module doc for why the model is bound by this rather than allowed to
/// re-decide it.
///
/// Every nested "is this optional / does it need a default" question below
/// is answered the same way: fields that describe the identity of *this*
/// watch (source, identity.strategy, predicate, …) are required,
/// because a contract missing one of those isn't a valid contract at all.
/// Fields more likely to grow across `contract_version` bumps default, so
/// that an older contract still deserializes cleanly against a newer schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchContract {
    pub contract_version: u32,
    /// RFC3339 timestamp of authoring. Excluded from [`WatchContract::fingerprint`] —
    /// it changes on every amendment without changing what the contract means.
    pub authored_at: String,
    /// Id of the run that authored (or last amended) this contract. Same
    /// exclusion from the fingerprint as `authored_at`, and for the same
    /// reason.
    pub authored_by_run: String,
    pub source: WatchSource,
    /// Answers "who is this?" — see [`IdentitySpec`].
    pub identity: IdentitySpec,
    /// Answers "has it changed in a way we care about?" — see [`ChangeSpec`].
    pub change: ChangeSpec,
    /// Answers "should it fire?" — see [`PredicateSpec`].
    pub predicate: PredicateSpec,
    #[serde(default)]
    pub mode: WatchMode,
    /// The extraction contract: every field the watch's agent is allowed to
    /// report, keyed by field name, with its type/required/normalize rules.
    /// A poll reporting a field not listed here, or missing one marked
    /// `required`, is what later pieces of this feature quarantine against.
    #[serde(default)]
    pub fields: HashMap<String, FieldSpec>,
}

impl WatchContract {
    /// Sha256, hex-encoded, of the canonical JSON of this contract with
    /// `authored_at`/`authored_by_run` excluded — those two fields change on
    /// every amendment without changing what the contract means, so
    /// including them would make every amendment look like a semantic change
    /// (and, worse, would make re-authoring an *identical* contract on retry
    /// produce a different fingerprint). This is the `contract_fingerprint`
    /// a later piece of this feature persists on the assignment scratchpad
    /// to detect that an amendment happened and a re-key is owed.
    pub fn fingerprint(&self) -> String {
        let mut value = serde_json::to_value(self).expect("WatchContract always serializes to JSON");
        if let Value::Object(map) = &mut value {
            map.remove("authored_at");
            map.remove("authored_by_run");
        }
        sha256_hex(&canonical_json(&value))
    }

    /// Structural validation independent of any particular observed payload:
    /// every invariant a contract must satisfy before it
    /// is ever trusted to key a poll. Authoring (a later piece of this
    /// feature, in `ao_engine::agent_watch`) runs this against every
    /// proposed contract before persisting it — a contract that fails this
    /// is never persisted, full stop.
    ///
    /// Does NOT check `identity.format` against any actually observed
    /// value — that needs real payloads from a poll and is authoring's own
    /// job, not a property this type can check in
    /// isolation.
    ///
    /// Does NOT check `predicate` for well-formedness either, unlike before
    /// `PredicateSpec::predicate` was typed: a `Predicate` value is
    /// well-formed by construction (there is no "unparsed" state it could be
    /// in), so there is nothing left to validate here. The one case that
    /// used to fail this check — an unparseable legacy `expr` string — is
    /// now caught earlier, at the point something still stringly-typed is
    /// converted into a `Predicate` (`legacy_expr::parse`, called from
    /// `PredicateSpec`'s deserializer for a persisted contract, and from
    /// `ao_engine::agent_watch::author_contract` for a freshly authored
    /// proposal).
    pub fn validate(&self) -> Result<(), ContractError> {
        if let Some(pattern) = self.identity.format.as_deref() {
            Regex::new(pattern).map_err(|e| ContractError::InvalidRegex(e.to_string()))?;
        }

        match self.identity.strategy {
            IdentityStrategy::NativeId => {
                if self.identity.source_field.as_deref().unwrap_or("").trim().is_empty() {
                    return Err(ContractError::EmptyIdentity);
                }
            }
            IdentityStrategy::CompositeNative | IdentityStrategy::ContentHash => {
                if self.identity.fields.is_empty() {
                    return Err(ContractError::EmptyIdentity);
                }
            }
        }

        // `NewOnly` fires on `prev.is_none()` alone (see `identity_key`'s
        // caller in `ao_engine::agent_watch`) — existence is the whole
        // event, so there is no prior version to diff and nothing for
        // `material_fields` to name. Every other mode fires off a version
        // transition, which is meaningless with no material fields declared
        // at all.
        if self.mode != WatchMode::NewOnly && self.change.material_fields.is_empty() {
            return Err(ContractError::EmptyMaterialFields);
        }

        // A field cannot simultaneously define what an item *is* and be
        // allowed to change: if it sits in both sets, every material edit
        // also mints a new identity_key, so "this item was updated" becomes
        // unrepresentable and every edit registers as a brand-new item
        // (see the module-level asymmetry note on identity width).
        let mut overlap: Vec<String> = self
            .identity
            .fields
            .iter()
            .filter(|field| self.change.material_fields.contains(field))
            .cloned()
            .collect();
        if !overlap.is_empty() {
            overlap.sort();
            overlap.dedup();
            return Err(ContractError::IdentityMaterialFieldOverlap(overlap));
        }

        // A field marked `required: true` and targeted by a `NotEmpty` leaf
        // anywhere in the predicate tree is a self-defeating pairing: the
        // required-field gate quarantines a blank value before the
        // predicate is ever evaluated, so `NotEmpty`'s whole reason for
        // existing — letting a blank value resolve to a quiet `false`
        // instead of firing — can never be reached. See
        // `ContractError::RequiredFieldTargetedByTolerantPredicate`.
        let mut not_empty_on_required_fields: Vec<String> = self
            .predicate
            .predicate
            .paths_matching(|p| matches!(p, Predicate::NotEmpty { .. }))
            .into_iter()
            .filter(|path| self.fields.get(*path).is_some_and(|spec| spec.required))
            .map(str::to_string)
            .collect();
        if !not_empty_on_required_fields.is_empty() {
            not_empty_on_required_fields.sort();
            not_empty_on_required_fields.dedup();
            // One offending field per error, same as `EmptyIdentity` etc.;
            // deterministic (sorted) choice when more than one field pairs
            // `required: true` with a `NotEmpty` leaf on itself.
            return Err(ContractError::RequiredFieldTargetedByTolerantPredicate(
                not_empty_on_required_fields.remove(0),
            ));
        }

        Ok(())
    }
}

/// What is being watched. `kind`/`ref` are free-form strings chosen by the
/// authoring agent (e.g. a connector kind and a native container id) — this
/// module never branches on their value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSource {
    pub kind: String,
    #[serde(rename = "ref")]
    pub ref_: String,
}

/// "Who is this?" — the fallback ladder described in the module doc. Exactly one
/// of `source_field` (rung 1) or `fields` (rungs 2–3) is meaningful for a
/// given [`IdentityStrategy`]; see [`identity_key`] for which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentitySpec {
    pub strategy: IdentityStrategy,
    /// The field the source itself exposes as a stable per-item key,
    /// discovered by the authoring agent (never a fixed name in code — see
    /// the module doc). Only meaningful for [`IdentityStrategy::NativeId`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_field: Option<String>,
    /// Regex the authoring agent derived from the values it actually
    /// observed for `source_field`. A value that fails this at runtime is a
    /// quarantine ([`ContractError::FormatMismatch`]), never a new item:
    /// failing closed is deliberate, because a value that no longer matches
    /// the observed shape far more often means the source changed than that
    /// something genuinely new arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Fields combined to form identity for [`IdentityStrategy::CompositeNative`]
    /// and [`IdentityStrategy::ContentHash`]; unused for `NativeId`.
    #[serde(default)]
    pub fields: Vec<String>,
    /// The authoring agent's own explanation of why this rung, recorded for
    /// the user-facing contract surface.
    #[serde(default)]
    pub rationale: String,
}

/// The identity fallback ladder. No source system is named
/// anywhere in this enum, in a comment, or in any error string produced by
/// this module — the ladder is generic by construction, and which rung a
/// given watch lands on is a fact recorded in data (the contract), not a
/// branch in code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStrategy {
    /// The source exposes one stable primary key; `identity.source_field`
    /// names it and `identity.format` (if present) bounds what a valid value
    /// looks like.
    NativeId,
    /// No single key, but a stable combination of fields; `identity.fields`
    /// lists them in the order they're combined.
    CompositeNative,
    /// No stable key exists at all; `identity.fields` lists the fields whose
    /// combined content stands in for identity.
    ContentHash,
}

/// "Has it changed in a way we care about?"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSpec {
    /// Fields whose change is meaningful enough to re-derive the version
    /// key. Deliberately a strict subset of `fields` — hashing every field
    /// would make an immaterial edit (fixing a typo in a notes column) look
    /// like a material one.
    #[serde(default)]
    pub material_fields: Vec<String>,
    /// A cheap pre-filter field the runtime tick may use to skip
    /// re-extraction when unchanged. Never part of any hash this module
    /// computes — see [`version_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_hint_field: Option<String>,
}

/// "Should it fire?" `predicate` is evaluated by
/// [`evaluate_predicate`]; `natural_language` and `fields` are carried for
/// the user-facing contract surface and are not otherwise interpreted here.
///
/// `predicate` used to be an `expr: String` compiled by a hand-rolled parser
/// on every use. [`Deserialize`] is implemented by hand below rather than
/// derived so that a contract persisted before this migration — whose JSON
/// still carries the legacy `expr` string instead of a `predicate` object —
/// keeps deserializing: the legacy field is parsed into the typed
/// [`Predicate`] via [`legacy_expr::parse`] on the way in. [`Serialize`]
/// is still derived normally, so anything this crate writes back out (an
/// amendment, a freshly authored contract) always serializes the typed
/// `predicate` field — there is no code path left that writes `expr`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PredicateSpec {
    #[serde(default)]
    pub natural_language: String,
    #[serde(default)]
    pub fields: Vec<String>,
    pub predicate: Predicate,
}

impl<'de> Deserialize<'de> for PredicateSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            natural_language: String,
            #[serde(default)]
            fields: Vec<String>,
            #[serde(default)]
            predicate: Option<Predicate>,
            /// Legacy field — present only on a contract persisted before
            /// this migration. See [`legacy_expr`].
            #[serde(default)]
            expr: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let predicate = match (wire.predicate, wire.expr) {
            (Some(predicate), _) => predicate,
            (None, Some(expr)) => legacy_expr::parse(&expr).map_err(serde::de::Error::custom)?,
            (None, None) => return Err(serde::de::Error::missing_field("predicate")),
        };
        Ok(PredicateSpec { natural_language: wire.natural_language, fields: wire.fields, predicate })
    }
}

/// How a matching item translates into a fire decision.
/// `PredicateTransition` is the v1 default: a false→true edge in
/// `predicate(payload)` fires exactly once, whether the edge comes from a
/// brand-new already-matching item or an existing item that just started
/// matching. `NewOrChanged`/`NewOnly` are coarser, level-triggered modes for
/// watches where transition semantics don't apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    #[default]
    PredicateTransition,
    NewOrChanged,
    NewOnly,
}

/// One field's extraction contract: what type it is, and whether a poll
/// missing it should be treated as an extraction failure. Normalization
/// before hashing/comparison is a fixed engine property (see
/// [`normalize_identity_text`]), not a per-field, contract-authorable knob —
/// a knob here would let identity keys vary per contract edit, which makes
/// them non-reproducible across amendments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSpec {
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
}

/// Quarantine signals for the fail-closed guardrail: any of
/// these means "I don't know whether this is new," which must never resolve
/// to firing. Callers surface these as a health event on the assignment
/// rather than treating the observation as a new item.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    /// A field the contract declares as load-bearing (an identity field, or
    /// `identity.source_field` itself) was absent or null in the payload.
    #[error("missing required field \"{0}\"")]
    MissingField(String),
    /// One or more `contract.fields` entries marked `required: true` were
    /// absent or null in an observed candidate. Unlike [`Self::MissingField`]
    /// (exactly one named field), this names every field missing from the
    /// same payload at once — comma-separated and individually quoted, so a
    /// candidate short several fields doesn't read as one bizarre field name
    /// (e.g. `"created_time, company, url"` mistaken for a single field).
    #[error(
        "missing required field{} {}",
        if .0.len() == 1 { "" } else { "s" },
        .0.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>().join(", ")
    )]
    MissingFields(Vec<String>),
    /// A `NativeId` value was present but failed `identity.format`. This is
    /// the check that turns a truncated or hallucinated relayed id into a
    /// quarantine instead of a silently-new item.
    #[error("field \"{field}\" value \"{value}\" does not match expected format /{pattern}/")]
    FormatMismatch { field: String, value: String, pattern: String },
    /// `identity.format` itself does not compile as a regex.
    #[error("invalid regex pattern: {0}")]
    InvalidRegex(String),
    /// `identity.fields` was empty for a strategy that requires it
    /// (`CompositeNative`/`ContentHash`), so there is nothing to hash.
    #[error("identity strategy produced an empty identity: no fields declared")]
    EmptyIdentity,
    /// `predicate.expr` failed to parse, or referenced an unknown function.
    /// Unlike a missing field inside a valid expression (which evaluates to
    /// `false`, not an error — see [`evaluate_predicate`]), this can never
    /// resolve to a fire decision: an unparseable predicate means the
    /// contract itself is broken.
    #[error("invalid predicate expression: {0}")]
    InvalidPredicate(String),
    /// `change.material_fields` was empty on a contract whose `mode` isn't
    /// [`WatchMode::NewOnly`]: nothing would ever be treated as a material
    /// version change, which is never what an authored contract should mean
    /// An authoring proposal with this shape is rejected outright rather
    /// than silently accepted as "never fires on a change".
    /// `WatchMode::NewOnly` is exempt — see [`WatchContract::validate`].
    #[error("no material fields declared — nothing would ever be treated as a version change")]
    EmptyMaterialFields,
    /// `identity.fields` and `change.material_fields` share at least one
    /// field: a field that identifies an item can never also be a field
    /// whose change is material, or every material edit re-keys the item's
    /// identity and looks like a brand-new item instead of a change to an
    /// existing one.
    #[error(
        "identity.fields and change.material_fields both contain {0:?} — a field that identifies an item cannot \
         also be a field whose change is material; move {0:?} to material_fields only (or drop it from \
         identity.fields entirely) and identify the item by its other, stable fields instead"
    )]
    IdentityMaterialFieldOverlap(Vec<String>),
    /// `fields[name].required` is `true` while `predicate.predicate` also
    /// targets `name` with a [`Predicate::NotEmpty`] leaf. The two are
    /// contradictory: `required` treats a present-but-blank value as
    /// *missing* and quarantines the candidate before any predicate ever
    /// runs (see the required-field gate in `ao_engine::agent_watch`), while
    /// `NotEmpty` exists specifically so a blank value evaluates to a quiet
    /// `false` instead of firing. Paired on the same field, the `NotEmpty`
    /// branch can never be reached — every blank value is quarantined first.
    /// The contract itself declares this contradiction; it isn't a mistake
    /// in whatever payload a poll later reports.
    #[error(
        "field {0:?} is declared required and is also targeted by a NotEmpty predicate — the contract contradicts \
         itself: required treats a blank value for {0:?} as missing and quarantines it before any predicate runs, \
         so the NotEmpty check's blank-tolerant `false` can never be reached; either drop `required` for {0:?}, or \
         remove the NotEmpty check on it"
    )]
    RequiredFieldTargetedByTolerantPredicate(String),
}

/// Bumped whenever a payload that reaches [`identity_key`] can end up keyed
/// differently than before — either because [`identity_key`]'s own hashing
/// rules changed (e.g. the identity-text normalization this constant was
/// introduced alongside, version 2), or because which payloads reach it at
/// all changed (version 3: `ao_engine::agent_watch`'s bind-mode reply parser
/// used to require every item to carry a top-level `id` field and silently
/// dropped every one that didn't, even though the bind-mode prompt itself
/// tells the model never to include one — so a contract-bound watch whose
/// `identity.strategy` was `composite_native` or `content_hash` (which have
/// no such field to coincidentally supply) parsed every poll down to zero
/// candidates, forever). A tick that finds a watch's persisted
/// `identity_keygen_version` doesn't match this must re-seed rather than
/// diff — every previously-stored key (or, for version 3, the absence of any
/// stored key at all) reflects the old rules, and a fresh key for the same
/// item is not "new."
pub const IDENTITY_KEYGEN_VERSION: u32 = 3;

/// Computes "who is this?" for one observed `payload`, per the ladder
/// declared in `contract.identity.strategy`. Sha256,
/// hex-encoded.
///
/// `NativeId` reads `payload[source_field]` and validates it against
/// `identity.format` if one is declared; a value that fails the regex is a
/// [`ContractError::FormatMismatch`], never silently accepted as a new id.
/// `CompositeNative` canonicalizes each declared field's value and
/// concatenates them (in declared order, unit-separator-joined so two
/// different field splits can't collide into the same string) before
/// hashing. `ContentHash` hashes the canonical JSON of the declared fields
/// taken together as one object. Both `CompositeNative` and `ContentHash`
/// fail closed ([`ContractError::MissingField`]) on any declared field that
/// is absent or null — a silently-changed identity is exactly the failure
/// mode the fail-closed guardrail exists to prevent.
pub fn identity_key(contract: &WatchContract, payload: &Value) -> Result<String, ContractError> {
    match contract.identity.strategy {
        IdentityStrategy::NativeId => native_id_key(contract, payload),
        IdentityStrategy::CompositeNative => composite_native_key(contract, payload),
        IdentityStrategy::ContentHash => content_hash_key(contract, payload),
    }
}

fn native_id_key(contract: &WatchContract, payload: &Value) -> Result<String, ContractError> {
    let field = contract
        .identity
        .source_field
        .as_deref()
        .ok_or_else(|| ContractError::MissingField("identity.source_field".to_string()))?;
    let raw = payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ContractError::MissingField(field.to_string()))?;
    if let Some(pattern) = contract.identity.format.as_deref() {
        let re = Regex::new(pattern).map_err(|e| ContractError::InvalidRegex(e.to_string()))?;
        if !re.is_match(&raw) {
            return Err(ContractError::FormatMismatch {
                field: field.to_string(),
                value: raw,
                pattern: pattern.to_string(),
            });
        }
    }
    Ok(sha256_hex(&normalize_identity_text(&raw)))
}

/// Unit separator between concatenated `CompositeNative` field values —
/// vanishingly unlikely to appear in real field content, and its purpose is
/// only to stop e.g. `["ab", "c"]` and `["a", "bc"]` from concatenating to
/// the same string, not to defend against adversarial input.
const COMPOSITE_KEY_JOIN: &str = "\u{1f}";

fn composite_native_key(contract: &WatchContract, payload: &Value) -> Result<String, ContractError> {
    if contract.identity.fields.is_empty() {
        return Err(ContractError::EmptyIdentity);
    }
    let mut parts = Vec::with_capacity(contract.identity.fields.len());
    for field in &contract.identity.fields {
        let value = payload
            .get(field)
            .filter(|v| !v.is_null())
            .ok_or_else(|| ContractError::MissingField(field.clone()))?;
        parts.push(canonical_json(&normalize_value_for_identity(value)));
    }
    Ok(sha256_hex(&parts.join(COMPOSITE_KEY_JOIN)))
}

fn content_hash_key(contract: &WatchContract, payload: &Value) -> Result<String, ContractError> {
    if contract.identity.fields.is_empty() {
        return Err(ContractError::EmptyIdentity);
    }
    let mut obj = serde_json::Map::new();
    for field in &contract.identity.fields {
        let value = payload
            .get(field)
            .filter(|v| !v.is_null())
            .ok_or_else(|| ContractError::MissingField(field.clone()))?;
        obj.insert(field.clone(), normalize_value_for_identity(value));
    }
    Ok(sha256_hex(&canonical_json(&Value::Object(obj))))
}

/// Computes "has it changed?" for one observed `payload`: sha256, hex
/// encoded, of the canonical JSON of `contract.change.material_fields` only
/// — deliberately ignoring every other field, including
/// `change.version_hint_field`, so an edit to a field nobody declared
/// material can never look like a version change. A
/// material field absent from `payload` contributes `null`, not an error:
/// unlike identity, "this field isn't here" is itself a stable, hashable
/// version state, not a quarantine condition.
pub fn version_key(contract: &WatchContract, payload: &Value) -> Result<String, ContractError> {
    let mut obj = serde_json::Map::new();
    for field in &contract.change.material_fields {
        let value = payload.get(field).map(normalize_value_for_hash).unwrap_or(Value::Null);
        obj.insert(field.clone(), value);
    }
    Ok(sha256_hex(&canonical_json(&Value::Object(obj))))
}

/// Evaluates `contract.predicate` against one observed `payload`.
/// `previous`, when the caller has it, is the same item's
/// payload as observed on a prior poll — pass it whenever one is available
/// (`ao_engine::agent_watch`'s tick loop has it via
/// `ItemSnapshot::payload`), since it's the only thing that makes
/// `Predicate::Changed` meaningful; pass `None` only when there genuinely is
/// no prior observation (a first-ever poll for this identity, or a
/// stand-alone evaluation with no snapshot history at all — see
/// `WatchContract::validate`'s doc for why this function no longer needs to
/// be called just to check `predicate` parses).
///
/// Total by construction — see [`crate::predicate::evaluate_predicate`].
pub fn evaluate_predicate(contract: &WatchContract, payload: &Value, previous: Option<&Value>) -> bool {
    evaluate_typed_predicate(&contract.predicate.predicate, payload, previous)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
