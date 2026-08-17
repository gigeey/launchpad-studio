//! A machine-executable contract for "watch this thing and tell me when it
//! changes," designed so that a poll can be resolved with zero model
//! involvement once the contract has been authored.
//!
//! An [`ExtractionPlan`] is `{ selector, identity, predicate }`. All three
//! tiers of confidence a watch can operate at — [`Tier::Deterministic`],
//! [`Tier::Probabilistic`], [`Tier::ChangeDetectionOnly`] — share this one
//! schema; only the [`ExtractorKind`] plugged into `selector`/`identity`
//! changes. There is deliberately only one pipeline here, not one per tier.
//!
//! [`enumerate_paths`] is the module's shape-report primitive: it tells an
//! authoring agent what a payload looks like (paths and types) without ever
//! showing it a value, so the agent is forced to author a path against a
//! declared shape rather than transcribe something it saw. [`resolve`] is
//! the binding entry point a later, stateful piece of this feature calls
//! every poll; its [`BindError`] is deliberately actionable (it carries the
//! available paths for a bad JsonPath) because it is itself the agent's
//! feedback loop for repairing a bad contract — but it is reserved for a
//! contract that cannot be evaluated at all; a selector that evaluates
//! cleanly and simply finds nothing this poll is `Ok` with zero items, not
//! an error.
//!
//! Everything in this module is pure: no I/O, no async, no clock reads, and
//! no persistence of prior state. A [`Predicate::Changed`] comparison needs
//! a previous value to compare against; [`resolve`] itself never has one
//! (a single poll's binding step has no history), so it always evaluates
//! predicates against `previous: None`. Tracking snapshots across polls and
//! calling [`evaluate_predicate`] again with a real `previous` value is a
//! later, stateful piece of this feature, not this module's job.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract_primitives::{canonical_json, normalize_text, sha256_hex};
/// `Predicate`/`evaluate_predicate`/`resolve_json_path` used to live in this
/// module; they moved to `crate::predicate` because they're a public type
/// `crate::watch_contract` also needs on its own persisted contracts — see
/// that module's doc for why. Re-exported here unchanged so every existing
/// reference in this module (and this module's own tests) keeps resolving
/// without qualification.
pub use crate::predicate::{evaluate_predicate, resolve_json_path, Predicate};

pub mod table;

/// Which mechanism extracts a value out of raw tool output. Also, one-to-one
/// except for [`Table`](ExtractorKind::Table) (see [`infer_tier`]'s own
/// doc), the ways a [`Tier`] can be reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ExtractorKind {
    /// Reads a value out of structured (JSON) content via the tiny dotted
    /// path dialect implemented by [`resolve_json_path`].
    JsonPath { path: String },
    /// Reads a value out of prose via a regular expression; `capture` names
    /// which capture group becomes the extracted value — a bare integer
    /// (`"0"`, `"1"`, ...) for a positional group, or a name for `(?P<name>...)`.
    Regex { pattern: String, capture: String },
    /// Extracts nothing; the whole payload is hashed as one opaque unit.
    /// Can only ever report that something changed, never what changed.
    Hash,
    /// Reads tabular markup — an HTML `<table>` or a markdown pipe table —
    /// out of the string field at `field_path` (the same dotted-path
    /// dialect [`resolve_json_path`] resolves; empty for the payload root
    /// itself) and yields one item per data row, each an object keyed by
    /// `columns` in column order. `columns` is fixed at authoring time
    /// (see [`table::discover_tabular_field`]) from the table's own header
    /// row: cell `i` of every data row becomes `columns[i]` in the
    /// resulting object, purely by position — the header text itself is
    /// never re-read once a plan is frozen, only its column count (a
    /// mismatch there means the source's shape drifted; see
    /// [`BindError::TableShapeChanged`]).
    ///
    /// `identity_columns` (a subset of `columns`, also frozen at authoring
    /// time) names which columns [`table::filter_blank_identity_rows`] treats
    /// as a row's identity: a row is dropped, never yielded as an item, when
    /// every one of these is blank — a table's own header-adjacent
    /// placeholder/template rows, not real data. `#[serde(default)]` so an
    /// already-persisted plan frozen before this field existed deserializes
    /// to an empty list, which is a no-op for that function (every row kept,
    /// unfiltered) rather than a hard error.
    Table {
        field_path: String,
        columns: Vec<String>,
        #[serde(default)]
        identity_columns: Vec<String>,
    },
}

/// How much an extracted value can be trusted, per [`infer_tier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A JsonPath read against content the server both returned as
    /// structured data *and* declared a schema for. The strongest
    /// guarantee this module can express: not just "there happened to be
    /// structured content this once," but "the server contractually
    /// promises this shape."
    Deterministic,
    /// Everything short of `Deterministic`: a JsonPath read against
    /// structured content the server never declared a schema for (it could
    /// change shape without notice), or any regex read over prose.
    Probabilistic,
    /// A [`ExtractorKind::Hash`] extractor. No value is ever recovered —
    /// only "the payload is byte-for-byte different from before."
    ChangeDetectionOnly,
}

/// Decides the confidence tier a `(selector, identity)` pairing is entitled
/// to claim, purely from what the tool call actually returned and declared —
/// never from wishful thinking baked into the contract itself.
///
/// - [`ExtractorKind::Hash`] is always [`Tier::ChangeDetectionOnly`],
///   regardless of what content is available: hashing throws away the
///   value, so there is never a stronger claim it could make.
/// - [`ExtractorKind::JsonPath`] is [`Tier::Deterministic`] only when the
///   response actually carried structured content *and* the server declared
///   an output schema for it — i.e. this shape is a promise, not a
///   one-time accident. Either bool being `false` drops it to
///   [`Tier::Probabilistic`].
/// - [`ExtractorKind::Regex`] is always [`Tier::Probabilistic`]: prose has
///   no schema to promise anything.
/// - [`ExtractorKind::Table`] is always [`Tier::Probabilistic`] too, for the
///   same reason as `Regex`: a server-declared output schema (if any)
///   describes the JSON envelope a table's markup happens to live inside,
///   never the markup's own row/column shape — that's never a promise the
///   server made, only a pattern this payload happened to match.
pub fn infer_tier(has_structured_content: bool, server_declared_output_schema: bool, kind: &ExtractorKind) -> Tier {
    match kind {
        ExtractorKind::Hash => Tier::ChangeDetectionOnly,
        ExtractorKind::JsonPath { .. } if has_structured_content && server_declared_output_schema => {
            Tier::Deterministic
        }
        ExtractorKind::JsonPath { .. } => Tier::Probabilistic,
        ExtractorKind::Regex { .. } => Tier::Probabilistic,
        ExtractorKind::Table { .. } => Tier::Probabilistic,
    }
}

/// One path in a [`enumerate_paths`] shape report: where a leaf or
/// container sits, and its type — one of `"string"`, `"number"`, `"bool"`,
/// `"null"`, `"array[n]"` (`n` is the array's length), or `"object"`. Never
/// the value itself; see [`enumerate_paths`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathInfo {
    pub path: String,
    pub value_type: String,
}

const DEFAULT_MAX_DEPTH: usize = 6;
const DEFAULT_MAX_PATHS: usize = 200;

/// The shape-report primitive: walks `v` and returns every path encountered
/// (bounded by `max_depth` levels of nesting and `max_paths` total entries)
/// together with its type only — never its value. This is deliberate: it is
/// shown to an agent so the agent is forced to author a path against a
/// declared shape rather than transcribe a value it happened to see.
///
/// A container (object/array) gets its own entry (so the caller can see it
/// exists and, for arrays, how long it is) in addition to its children being
/// walked; a scalar gets exactly one entry, itself. Once `max_paths` entries
/// have been collected, traversal stops immediately, even mid-object.
pub fn enumerate_paths(v: &Value, max_depth: usize, max_paths: usize) -> Vec<PathInfo> {
    let mut out = Vec::new();
    walk_shape(v, String::new(), 0, max_depth, max_paths, &mut out);
    out
}

fn walk_shape(value: &Value, path: String, depth: usize, max_depth: usize, max_paths: usize, out: &mut Vec<PathInfo>) {
    if out.len() >= max_paths {
        return;
    }
    match value {
        Value::Object(map) => {
            if !path.is_empty() {
                out.push(PathInfo { path: path.clone(), value_type: "object".to_string() });
            }
            if out.len() >= max_paths || depth >= max_depth {
                return;
            }
            for (key, child) in map {
                let child_path = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                walk_shape(child, child_path, depth + 1, max_depth, max_paths, out);
                if out.len() >= max_paths {
                    return;
                }
            }
        }
        Value::Array(items) => {
            out.push(PathInfo { path: path.clone(), value_type: format!("array[{}]", items.len()) });
            if out.len() >= max_paths || depth >= max_depth {
                return;
            }
            for (i, child) in items.iter().enumerate() {
                let child_path = format!("{path}[{i}]");
                walk_shape(child, child_path, depth + 1, max_depth, max_paths, out);
                if out.len() >= max_paths {
                    return;
                }
            }
        }
        scalar => {
            let value_type = match scalar {
                Value::String(_) => "string",
                Value::Number(_) => "number",
                Value::Bool(_) => "bool",
                Value::Null => "null",
                _ => unreachable!("Object/Array handled above"),
            };
            out.push(PathInfo { path, value_type: value_type.to_string() });
        }
    }
}

/// What is enumerated: `kind` names the extraction mechanism, `expr` is the
/// one expression that mechanism evaluates to yield 0..N item payloads —
/// for [`ExtractorKind::JsonPath`], a dotted path into `structured` (an
/// array result yields one item per element, a scalar/object result yields
/// exactly one item); for [`ExtractorKind::Regex`], a pattern applied to
/// `text` (every match yields one item, its full matched text —
/// [`ExtractorKind::Regex`]'s own `capture` field is for [`ExtractionPlan::identity`]
/// to pick a sub-piece of an already-selected item, not for selection
/// itself); [`ExtractorKind::Hash`] always yields exactly one item, the
/// whole payload; [`ExtractorKind::Table`] yields one item per data row of
/// whichever single recognizable table sits at its own `field_path`.
///
/// `kind`'s own inner fields (`path`, `pattern`, `capture`) are **not**
/// consulted when `kind` sits inside a `Selector` — `expr` is the sole
/// authoritative expression here, so that a selector's expression and an
/// identity/predicate path never have two different places to look for the
/// "real" string. `kind`'s inner fields are read normally everywhere else
/// `ExtractorKind` appears standalone (i.e. as [`ExtractionPlan::identity`]).
/// [`ExtractorKind::Table`] is the one exception: its `columns` field can't
/// be flattened into a single `expr` string, so both of its own inner
/// fields ARE read directly off `kind` when it sits inside a `Selector`;
/// `expr` still mirrors its `field_path` for continuity with the other
/// variants, but is not authoritative for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Selector {
    pub kind: ExtractorKind,
    pub expr: String,
}

/// The full watch contract: what to look at (`selector`), how to name each
/// thing found (`identity`), and what counts as a match (`predicate`). See
/// the module doc for how the three tiers share this one schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionPlan {
    pub selector: Selector,
    pub identity: ExtractorKind,
    pub predicate: Predicate,
}

/// One item [`resolve`] found: its stable id (from [`ExtractionPlan::identity`]),
/// its extracted payload, and whether [`ExtractionPlan::predicate`] currently
/// holds for it (evaluated with `previous: None` — see the module doc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedItem {
    pub id: String,
    pub value: Value,
    pub matched: bool,
}

/// The result of binding an [`ExtractionPlan`] against one poll's content:
/// every item the selector found, each with its identity and predicate
/// result attached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub items: Vec<ResolvedItem>,
}

/// [`resolve`]'s failure mode — deliberately actionable rather than a bare
/// "not found," since this is the agent's feedback loop for repairing a
/// contract that doesn't match what a tool actually returns.
///
/// A `Regex`-kind expression that simply matches zero times is *not* one of
/// these — that's the ordinary "nothing new this poll" case and resolves as
/// `Ok` with zero items, the same as a JsonPath selector over an empty
/// array. Every variant here means the contract itself cannot be evaluated
/// against what it was given, not that evaluation found nothing.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BindError {
    /// The selector needs structured content, or needs text, and `resolve`
    /// was given `None` for it.
    #[error("the contract's selector needs content that resolve() was not given")]
    NoContentSupplied,
    /// A `Regex`-kind expression does not compile.
    #[error("pattern \"{pattern}\" is not a valid regex: {reason}")]
    InvalidPattern { pattern: String, reason: String },
    /// A `JsonPath`-kind expression did not resolve against the supplied
    /// structured content. Carries every path [`enumerate_paths`] found in
    /// that content (types only, never values) so the caller can author a
    /// path against the shape that's actually there.
    #[error("path \"{path}\" did not resolve against the supplied structured content")]
    StructuredPathNotFound { path: String, available_paths: Vec<PathInfo> },
    /// A `Table`-kind selector's `field_path` resolved to content, but that
    /// content no longer contains exactly one recognizable table (HTML or
    /// markdown), or its header cell count no longer matches the frozen
    /// plan's own `columns` — either way, the field's shape drifted since
    /// the plan was authored and a positional cell-to-column mapping can no
    /// longer be trusted.
    #[error("field \"{field_path}\" no longer contains exactly one recognizable table with the expected column count")]
    TableShapeChanged { field_path: String },
}

fn extract_capture(captures: &regex::Captures, capture: &str) -> Option<String> {
    if let Ok(index) = capture.parse::<usize>() {
        captures.get(index).map(|m| m.as_str().to_string())
    } else {
        captures.name(capture).map(|m| m.as_str().to_string())
    }
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => canonical_json(other),
    }
}

fn select_items(selector: &Selector, structured: Option<&Value>, text: Option<&str>) -> Result<Vec<Value>, BindError> {
    match &selector.kind {
        ExtractorKind::JsonPath { .. } => {
            let root = structured.ok_or(BindError::NoContentSupplied)?;
            match resolve_json_path(root, &selector.expr) {
                Some(Value::Array(items)) => Ok(items.clone()),
                Some(other) => Ok(vec![other.clone()]),
                None => Err(BindError::StructuredPathNotFound {
                    path: selector.expr.clone(),
                    available_paths: enumerate_paths(root, DEFAULT_MAX_DEPTH, DEFAULT_MAX_PATHS),
                }),
            }
        }
        ExtractorKind::Regex { .. } => {
            let raw = text.ok_or(BindError::NoContentSupplied)?;
            let normalized = normalize_text(raw);
            let re = Regex::new(&selector.expr)
                .map_err(|e| BindError::InvalidPattern { pattern: selector.expr.clone(), reason: e.to_string() })?;
            // Zero matches is the ordinary case for a watch whose whole point
            // is that most polls find nothing new — it is not a broken
            // pattern, so it resolves the same as a JsonPath selector that
            // yields an empty array: `Ok` with zero items, never an `Err`.
            let items: Vec<Value> =
                re.find_iter(&normalized).map(|m| Value::String(m.as_str().to_string())).collect();
            Ok(items)
        }
        ExtractorKind::Hash => {
            if let Some(root) = structured {
                Ok(vec![root.clone()])
            } else if let Some(raw) = text {
                Ok(vec![Value::String(normalize_text(raw))])
            } else {
                Err(BindError::NoContentSupplied)
            }
        }
        ExtractorKind::Table { field_path, columns, identity_columns } => {
            let root = structured.ok_or(BindError::NoContentSupplied)?;
            let markup = match resolve_json_path(root, field_path) {
                Some(Value::String(s)) => s.as_str(),
                _ => {
                    return Err(BindError::StructuredPathNotFound {
                        path: field_path.clone(),
                        available_paths: enumerate_paths(root, DEFAULT_MAX_DEPTH, DEFAULT_MAX_PATHS),
                    });
                }
            };
            let tables = table::find_tables_in_text(markup);
            let [found] = tables.as_slice() else {
                return Err(BindError::TableShapeChanged { field_path: field_path.clone() });
            };
            if found.header.len() != columns.len() {
                return Err(BindError::TableShapeChanged { field_path: field_path.clone() });
            }
            let rows: Vec<Value> = found.rows.iter().map(|row| table::row_to_value(columns, row)).collect();
            Ok(table::filter_blank_identity_rows(rows, identity_columns))
        }
    }
}

fn compute_identity(kind: &ExtractorKind, item: &Value) -> String {
    match kind {
        ExtractorKind::JsonPath { path } => {
            resolve_json_path(item, path).map(canonical_json).unwrap_or_else(|| "null".to_string())
        }
        ExtractorKind::Regex { pattern, capture } => {
            let normalized = normalize_text(&value_as_text(item));
            Regex::new(pattern)
                .ok()
                .and_then(|re| re.captures(&normalized))
                .and_then(|captures| extract_capture(&captures, capture))
                .unwrap_or_default()
        }
        ExtractorKind::Hash => sha256_hex(&normalize_text(&value_as_text(item))),
        // `Table` is a selector-only mechanism (see its own doc) — Tier 2
        // authoring never sets `ExtractionPlan::identity` to `Table`,
        // choosing `JsonPath`/`Hash` against an already-selected row exactly
        // like the Array/Object selector branches do — so this arm exists
        // only for match exhaustiveness. Hashing the whole item is the same
        // fallback the `Hash` arm above already uses.
        ExtractorKind::Table { .. } => sha256_hex(&normalize_text(&value_as_text(item))),
    }
}

/// Binds an [`ExtractionPlan`] against one poll's content: runs `selector`
/// to find 0..N item payloads, then `identity` and `predicate` (with
/// `previous: None`, per the module doc) against each one.
///
/// Fails with an actionable [`BindError`] when the selector's expression
/// itself cannot be evaluated against the supplied content — an invalid
/// regex, or a JsonPath that doesn't resolve at all — since that's a
/// contract bug the caller needs to see and repair, not silence. A selector
/// that evaluates cleanly and simply matches nothing this poll (a JsonPath
/// onto an empty array, or a regex with zero matches) is `Ok` with zero
/// items: the ordinary case for a watch, not a failure.
pub fn resolve(contract: &ExtractionPlan, structured: Option<&Value>, text: Option<&str>) -> Result<Resolution, BindError> {
    let items = select_items(&contract.selector, structured, text)?;
    let resolved = items
        .into_iter()
        .map(|item| {
            let id = compute_identity(&contract.identity, &item);
            let matched = evaluate_predicate(&contract.predicate, &item, None);
            ResolvedItem { id, value: item, matched }
        })
        .collect();
    Ok(Resolution { items: resolved })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
