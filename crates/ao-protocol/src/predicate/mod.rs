//! The typed predicate DSL shared by [`crate::watch_contract`] and
//! [`crate::extractor_contract`]: a closed set of leaf comparisons against a
//! JSON path, combined with `And`/`Or`/`Not`.
//!
//! Lives in its own top-level module rather than [`crate::contract_primitives`]
//! because it is a public type both modules expose on their persisted
//! contracts (`watch_contract::PredicateSpec`, `extractor_contract::ExtractionPlan`)
//! — `contract_primitives` is `pub(crate)` by design (raw hashing/normalization
//! helpers never meant to leak outside this crate), which a public-facing
//! predicate type cannot live inside.
//!
//! [`resolve_json_path`] (the tiny dotted-path-plus-bracketed-index dialect
//! every leaf variant resolves through) lives here too, not in
//! `extractor_contract`, for the same reason: `Predicate::Equals`/`Changed`/
//! etc. are defined in terms of it, and a flat field name (no dots, no
//! brackets — the only path shape `watch_contract`'s legacy string grammar
//! ever produced) resolves through it exactly like a single object-key
//! lookup, so nothing about promoting it here changes what a bare identifier
//! means. `extractor_contract` re-exports both from here for its own
//! selector/identity extraction, which also needs path resolution.
//!
//! `NotEmpty`/`ContainsCi`/`EqualsCi` are not part of `extractor_contract`'s
//! original vocabulary — they exist to give `watch_contract`'s legacy string
//! grammar (`not_empty(field)`, case-insensitive `contains`/`equals`) an
//! exact typed equivalent, so migrating a persisted contract's `expr` string
//! into this enum (see `watch_contract::legacy_expr`) never has to
//! approximate or silently change a watch's firing behavior. They stay
//! case-insensitive and string-coercing by definition, deliberately distinct
//! from `Contains`/`Equals` (case-sensitive, typed-value comparison) rather
//! than changing those two variants' existing semantics.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A closed predicate DSL — no free-text, no model evaluation. Every variant
/// is either a leaf comparison against a JSON path (resolved via
/// [`resolve_json_path`]) or a boolean combinator over nested predicates.
/// Serde-serializable and round-trippable; uses the default (externally
/// tagged) enum representation rather than an internal tag, since `And`/`Or`
/// wrap a `Vec` rather than a struct-shaped payload and internally tagged
/// enums require every variant to serialize as a JSON object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Predicate {
    Exists { path: String },
    Equals { path: String, value: Value },
    NotEquals { path: String, value: Value },
    Contains { path: String, substring: String },
    GreaterThan { path: String, number: f64 },
    LessThan { path: String, number: f64 },
    /// True when the value at `path` differs from what it was on the
    /// previous observation. Meaningless without a `previous` value — see
    /// [`evaluate_predicate`]'s `previous` parameter — and always `true`
    /// when there is no previous observation to compare against (a
    /// first-ever observation is, definitionally, a difference from
    /// nothing).
    Changed { path: String },
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
    /// True if `path` resolves to a non-blank string, a non-empty array, a
    /// non-empty object, or any scalar other than `null` — absent/`null`
    /// resolves to `false`. Deliberately distinct from `Exists` (which
    /// treats an empty string/array/object as present) — see the module doc.
    NotEmpty { path: String },
    /// Case-insensitive substring match against `path`'s value, coerced to
    /// its `Display` text when not already a string. See the module doc.
    ContainsCi { path: String, literal: String },
    /// Case-insensitive equality against `path`'s value, coerced to its
    /// `Display` text when not already a string. See the module doc.
    EqualsCi { path: String, literal: String },
}

impl Predicate {
    /// Renders this predicate back into the string grammar
    /// [`crate::watch_contract::legacy_expr::parse`] accepts, for display in
    /// the user-facing contract explainer ("show raw expression").
    ///
    /// This is a *diagnostic projection of what actually executes*, not a
    /// recovery of whatever string a contract was originally authored from.
    /// Nothing persists the result and nothing parses it back in anger — the
    /// runtime always evaluates the typed [`Predicate`] directly. That
    /// direction matters: a stored-and-replayed original string could drift
    /// out of sync with the typed value it was parsed into, and would then be
    /// showing a user an expression their watch is not running.
    ///
    /// Returns `None` — never a lossy or unparseable approximation — whenever
    /// this predicate cannot be expressed in that grammar. The typed enum is
    /// strictly richer than the grammar, so this is a real, reachable case,
    /// not defensive padding:
    ///
    /// - [`Predicate::Exists`], [`Predicate::Equals`], [`Predicate::NotEquals`],
    ///   [`Predicate::Contains`], [`Predicate::GreaterThan`],
    ///   [`Predicate::LessThan`] and [`Predicate::Changed`] have no spelling in
    ///   the grammar at all (it only ever had case-insensitive
    ///   `contains`/`equals`, `not_empty`, and the three combinators).
    /// - A `path` the grammar's identifier rule cannot express — anything
    ///   outside `[alphanumeric_]`, including the dotted/indexed paths
    ///   [`resolve_json_path`] supports (`a.b[0]`), and the empty path.
    /// - An empty [`Predicate::And`]/[`Predicate::Or`], whose vacuous
    ///   `true`/`false` the grammar has no constant for.
    ///
    /// Callers must surface `None` as an explicit "no expression available"
    /// rather than a blank: a silent empty box is exactly the failure this
    /// method exists to remove.
    ///
    /// `And`/`Or` are n-ary here but strictly binary in the grammar, so a
    /// 3-or-more-element combinator right-folds into nested binary form
    /// (`and(a, and(b, c))`) and a 1-element one renders as its sole operand.
    /// Both are evaluation-identical to the original (see this module's
    /// tests), but only the exactly-2-element form parses back to a
    /// structurally identical [`Predicate`].
    pub fn to_expr(&self) -> Option<String> {
        match self {
            Predicate::NotEmpty { path } => Some(format!("not_empty({})", grammar_ident(path)?)),
            Predicate::ContainsCi { path, literal } => {
                Some(format!("contains({}, '{}')", grammar_ident(path)?, escape_literal(literal)))
            }
            Predicate::EqualsCi { path, literal } => {
                Some(format!("equals({}, '{}')", grammar_ident(path)?, escape_literal(literal)))
            }
            Predicate::Not(inner) => Some(format!("not({})", inner.to_expr()?)),
            Predicate::And(operands) => fold_binary("and", operands),
            Predicate::Or(operands) => fold_binary("or", operands),
            Predicate::Exists { .. }
            | Predicate::Equals { .. }
            | Predicate::NotEquals { .. }
            | Predicate::Contains { .. }
            | Predicate::GreaterThan { .. }
            | Predicate::LessThan { .. }
            | Predicate::Changed { .. } => None,
        }
    }

    /// The `path` this node reads, for a leaf variant — `None` for `And`/
    /// `Or`/`Not`, which have no path of their own (see
    /// [`Predicate::paths_matching`] for descending into their children).
    /// Every leaf variant carries a `path: String`, so this is exhaustive
    /// without needing a catch-all arm.
    fn leaf_path(&self) -> Option<&str> {
        match self {
            Predicate::Exists { path }
            | Predicate::Equals { path, .. }
            | Predicate::NotEquals { path, .. }
            | Predicate::Contains { path, .. }
            | Predicate::GreaterThan { path, .. }
            | Predicate::LessThan { path, .. }
            | Predicate::Changed { path }
            | Predicate::NotEmpty { path }
            | Predicate::ContainsCi { path, .. }
            | Predicate::EqualsCi { path, .. } => Some(path.as_str()),
            Predicate::And(_) | Predicate::Or(_) | Predicate::Not(_) => None,
        }
    }

    /// Collects the `path` of every leaf in this predicate's tree for which
    /// `is_match` returns `true`, recursing through `And`/`Or`/`Not` (a leaf
    /// nested arbitrarily deep inside combinators is found exactly like one
    /// at the root). Generic over which leaf variant(s) count as a match so
    /// this serves any "which fields does this predicate reference" question
    /// — a caller wanting every referenced field regardless of variant can
    /// pass `|_| true`; [`crate::watch_contract::WatchContract::validate`]
    /// passes a filter matching only [`Predicate::NotEmpty`], since that is
    /// the one variant whose blank-tolerant `false` a `required: true` field
    /// would make unreachable.
    pub fn paths_matching(&self, is_match: impl Fn(&Predicate) -> bool + Copy) -> Vec<&str> {
        match self {
            Predicate::And(predicates) | Predicate::Or(predicates) => {
                predicates.iter().flat_map(|p| p.paths_matching(is_match)).collect()
            }
            Predicate::Not(inner) => inner.paths_matching(is_match),
            leaf => {
                if is_match(leaf) {
                    leaf.leaf_path().into_iter().collect()
                } else {
                    Vec::new()
                }
            }
        }
    }
}

/// `path` unchanged if the grammar's identifier rule can express it, else
/// `None`. Mirrors that parser's accepted character set exactly (alphanumeric
/// or `_`, at least one character) — the two must agree or [`Predicate::to_expr`]
/// could emit a string that fails to parse back.
fn grammar_ident(path: &str) -> Option<&str> {
    (!path.is_empty() && path.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(path)
}

/// Escapes a literal for the grammar's single-quoted string form. Backslash
/// must be escaped before quote, otherwise an already-present backslash would
/// be re-read as the escape for a following quote.
fn escape_literal(literal: &str) -> String {
    literal.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Right-folds an n-ary combinator into the grammar's binary form. Empty is
/// `None` (no constant to render); a single operand renders as itself.
fn fold_binary(name: &str, operands: &[Predicate]) -> Option<String> {
    let (first, rest) = operands.split_first()?;
    if rest.is_empty() {
        return first.to_expr();
    }
    Some(format!("{name}({}, {})", first.to_expr()?, fold_binary(name, rest)?))
}

/// Evaluates `predicate` against `value` (typically one item's payload).
/// `previous`, if given, is the same item's payload as observed on a prior
/// poll — only [`Predicate::Changed`] reads it; every other variant ignores
/// it entirely. A path that doesn't resolve, or resolves to the wrong type
/// for the comparison being made, evaluates to `false` rather than erroring
/// — this function is total by construction, never fails, and never treats
/// "couldn't tell" as `true`.
pub fn evaluate_predicate(predicate: &Predicate, value: &Value, previous: Option<&Value>) -> bool {
    match predicate {
        Predicate::Exists { path } => resolve_json_path(value, path).is_some_and(|v| !v.is_null()),
        Predicate::Equals { path, value: expected } => resolve_json_path(value, path) == Some(expected),
        Predicate::NotEquals { path, value: expected } => resolve_json_path(value, path) != Some(expected),
        Predicate::Contains { path, substring } => resolve_json_path(value, path)
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains(substring.as_str())),
        Predicate::GreaterThan { path, number } => {
            resolve_json_path(value, path).and_then(Value::as_f64).is_some_and(|n| n > *number)
        }
        Predicate::LessThan { path, number } => {
            resolve_json_path(value, path).and_then(Value::as_f64).is_some_and(|n| n < *number)
        }
        Predicate::Changed { path } => {
            let current = resolve_json_path(value, path);
            let prior = previous.and_then(|p| resolve_json_path(p, path));
            current != prior
        }
        Predicate::And(predicates) => predicates.iter().all(|p| evaluate_predicate(p, value, previous)),
        Predicate::Or(predicates) => predicates.iter().any(|p| evaluate_predicate(p, value, previous)),
        Predicate::Not(inner) => !evaluate_predicate(inner, value, previous),
        Predicate::NotEmpty { path } => match resolve_json_path(value, path) {
            Some(Value::Null) | None => false,
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
            Some(_) => true,
        },
        Predicate::ContainsCi { path, literal } => field_as_text(value, path)
            .map(|v| v.to_lowercase().contains(&literal.to_lowercase()))
            .unwrap_or(false),
        Predicate::EqualsCi { path, literal } => field_as_text(value, path)
            .map(|v| v.to_lowercase() == literal.to_lowercase())
            .unwrap_or(false),
    }
}

/// String coercion shared by [`Predicate::ContainsCi`]/[`Predicate::EqualsCi`]:
/// a string resolves as itself, a present-but-non-string/non-null value
/// coerces via `Display`, and a missing or `null` value has no text to
/// compare (`None`, never an empty string — the caller's `unwrap_or(false)`
/// is what turns "no text" into "no match").
fn field_as_text(value: &Value, path: &str) -> Option<String> {
    match resolve_json_path(value, path) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

enum PathSegment<'a> {
    Key(&'a str),
    Index(usize),
}

/// Parses `path` into segments, or `None` if it isn't well-formed. Total —
/// never panics on malformed input.
fn parse_path_segments(path: &str) -> Option<Vec<PathSegment<'_>>> {
    let mut segments = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(stripped) = rest.strip_prefix('.') {
            // A dot with nothing (or another dot) immediately after it is an
            // empty segment — malformed, not "skip it and keep going".
            if stripped.is_empty() || stripped.starts_with('.') {
                return None;
            }
            rest = stripped;
            continue;
        }
        if let Some(stripped) = rest.strip_prefix('[') {
            let end = stripped.find(']')?;
            let index: usize = stripped[..end].parse().ok()?;
            segments.push(PathSegment::Index(index));
            rest = &stripped[end + 1..];
            continue;
        }
        let end = rest.find(['.', '[']).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        segments.push(PathSegment::Key(&rest[..end]));
        rest = &rest[end..];
    }
    Some(segments)
}

/// Resolves a tiny JsonPath dialect against `root`: dot-separated object
/// keys and bracketed numeric array indices only — e.g. `a.b[0].c`. No
/// wildcards, no filters, no recursive descent — a malformed path or a
/// missing key/index simply resolves to `None`, never a panic. An empty
/// path resolves to `root` itself. A bare identifier with no dots or
/// brackets (`watch_contract`'s legacy grammar never produced anything
/// else) resolves exactly like a single object-key lookup.
pub fn resolve_json_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(root);
    }
    let segments = parse_path_segments(path)?;
    let mut current = root;
    for segment in segments {
        current = match segment {
            PathSegment::Key(key) => current.as_object()?.get(key)?,
            PathSegment::Index(index) => current.as_array()?.get(index)?,
        };
    }
    Some(current)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
