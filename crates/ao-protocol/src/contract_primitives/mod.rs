//! Low-level primitives shared by [`crate::watch_contract`] and
//! [`crate::extractor_contract`] — deterministic JSON canonicalization,
//! hashing, and the cosmetic-variance normalizers both contract types need
//! before a value is safe to hash or compare. Also `pub` at the crate
//! boundary for `ao_engine::agent_watch`'s legacy (pre-`WatchContract`)
//! fallback, which needs the exact same "normalize, canonicalize, hash"
//! recipe to derive a content-based dedupe key instead of trusting a
//! detector's own free-text item id.
//!
//! `crate::watch_contract`'s `identity_key`/`version_key` outputs are
//! persisted on disk (`AssignmentScratchpad.snapshots`); every function here
//! that feeds those hashes must stay byte-for-byte identical to what it
//! produced before this module existed. This module only relocates code —
//! see each function's origin note for where it moved from.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Deterministic string form of a `serde_json::Value`: object keys sorted, no
/// insignificant whitespace, numbers rendered via `serde_json`'s own
/// canonical `Display`. Two calls observing values that are structurally
/// identical but built/serialized in a different key order MUST produce
/// byte-identical output — every hash `watch_contract` computes depends on
/// it, and `extractor_contract` relies on the same determinism to render a
/// non-string item payload as a stable id.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&serde_json::to_string(s).expect("a string always serializes to JSON")),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("a key always serializes to JSON"));
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
    }
}

/// Sha256, hex-encoded.
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Cosmetic Unicode confusables folded to their ASCII equivalents, internal
/// whitespace runs collapsed to a single space, and the result lowercased,
/// before a string ever contributes to a `watch_contract` *identity* hash
/// (`identity_key`) — a source that silently swaps `'` for a typographic
/// `'`, pads a value with a non-breaking space, or changes case between
/// polls must not mint a new identity for content nobody actually changed.
/// Deliberately not used for `watch_contract`'s `version_key`: a version
/// hash's job is to notice material edits, and folding away case/whitespace
/// variance there would hide changes a user asked to be told about. Not full
/// Unicode NFC: composing decomposed characters needs a decomposition table
/// this function doesn't carry, and this covers the variance observed in
/// practice.
pub fn normalize_identity_text(s: &str) -> String {
    let mut folded = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' | '\u{00B4}' => folded.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{2033}' => folded.push('"'),
            '\u{2010}'..='\u{2015}' | '\u{2212}' => folded.push('-'),
            '\u{00A0}' | '\u{2007}' | '\u{202F}' => folded.push(' '),
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => {}
            other => folded.push(other),
        }
    }
    folded.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Recursively applies [`normalize_identity_text`] to every string leaf of a
/// JSON value, leaving structure and non-string leaves untouched — used to
/// normalize a `CompositeNative`/`ContentHash` identity field's value before
/// it is folded into an identity hash.
pub fn normalize_value_for_identity(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(normalize_identity_text(s)),
        Value::Array(items) => Value::Array(items.iter().map(normalize_value_for_identity).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), normalize_value_for_identity(v))).collect())
        }
        other => other.clone(),
    }
}

/// Curly single/double quotes folded to their ASCII equivalents, and the
/// value trimmed, before it ever contributes to `watch_contract`'s version
/// hash — a source that silently swaps `'` for a typographic `'` (or pads a
/// value with whitespace) between polls must not look like a material edit
/// nobody actually made. Deliberately narrower than
/// [`normalize_identity_text`] (no case-folding, no confusable-dash/NBSP
/// handling): a version hash's job is to notice material changes, so it only
/// folds away variance that is purely cosmetic rendering noise, never
/// variance a user might consider a real edit.
pub(crate) fn fold_quotes_and_trim(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            other => other,
        })
        .collect()
}

/// Recursively applies [`fold_quotes_and_trim`] to every string leaf of a
/// JSON value, leaving structure and non-string leaves untouched — used to
/// normalize a material field's value before it is folded into
/// `watch_contract`'s version-key hash.
pub(crate) fn normalize_value_for_hash(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(fold_quotes_and_trim(s)),
        Value::Array(items) => Value::Array(items.iter().map(normalize_value_for_hash).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), normalize_value_for_hash(v))).collect()),
        other => other.clone(),
    }
}

/// Normalizes prose before `extractor_contract` matches
/// (`ExtractorKind::Regex`) or hashes (`ExtractorKind::Hash`) it: Unicode
/// NFKC, smart quotes and unicode dashes straightened to their ASCII
/// equivalents, then internal whitespace runs collapsed to a single space.
///
/// This is NOT the same normalizer as [`normalize_identity_text`], even
/// though both fold quote/dash confusables and collapse whitespace — they
/// diverge in three ways that make merging them behavior-changing rather
/// than cosmetic:
/// - This function applies Unicode NFKC decomposition first (e.g. the
///   ligature `ﬁ` becomes `fi`); `normalize_identity_text` does not.
/// - This function never lowercases; `normalize_identity_text` always does
///   (`extractor_contract`'s regex/hash matching wants case preserved —
///   `evaluate_predicate`'s `Contains`/`Equals` do their own
///   case-insensitive comparison where that's wanted).
/// - `normalize_identity_text` also strips zero-width characters and folds
///   NBSP/narrow-no-break-space to a plain space; this function leaves them
///   as NFKC resolves them.
///
/// Deliberately never applied to a `JsonPath` read in either module —
/// comparing typed JSON values (a number is a number, a string is exactly
/// the bytes it is) needs none of this, and applying it there would be both
/// dead weight and a correctness hazard (silently changing what a value
/// compares equal to).
pub(crate) fn normalize_text(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let nfkc: String = input.nfkc().collect();
    let straightened: String = nfkc
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' | '\u{00B4}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{2033}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            other => other,
        })
        .collect();
    straightened.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
