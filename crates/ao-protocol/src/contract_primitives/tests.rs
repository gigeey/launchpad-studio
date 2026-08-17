use super::*;
use serde_json::json;

// ---- canonical_json --------------------------------------------------------

#[test]
fn canonical_json_sorts_object_keys_and_has_no_insignificant_whitespace() {
    let a = json!({ "b": 1, "a": 2, "c": 3 });
    let b = json!({ "c": 3, "a": 2, "b": 1 });
    assert_eq!(canonical_json(&a), canonical_json(&b));
    assert_eq!(canonical_json(&a), r#"{"a":2,"b":1,"c":3}"#);
}

#[test]
fn canonical_json_is_deterministic_under_deep_key_reordering() {
    let a = json!({
        "outer": { "z": 1, "y": { "n": true, "m": null }, "x": [1, 2, 3] },
        "top": "value"
    });
    let b = json!({
        "top": "value",
        "outer": { "x": [1, 2, 3], "y": { "m": null, "n": true }, "z": 1 }
    });
    assert_eq!(canonical_json(&a), canonical_json(&b));
}

// ---- sha256_hex -------------------------------------------------------------

#[test]
fn sha256_hex_is_deterministic_and_a_64_char_hex_digest() {
    let a = sha256_hex("hello world");
    let b = sha256_hex("hello world");
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, sha256_hex("hello world!"));
}

// ---- normalize_identity_text / normalize_value_for_identity ----------------

#[test]
fn normalize_identity_text_folds_confusables_collapses_whitespace_and_lowercases() {
    assert_eq!(normalize_identity_text("O\u{2019}Brien"), "o'brien");
    assert_eq!(normalize_identity_text("Peter\u{00A0}Corp"), "peter corp");
    assert_eq!(normalize_identity_text("  A   B  "), "a b");
    assert_eq!(normalize_identity_text("CAFE\u{200B}"), "cafe");
}

#[test]
fn normalize_value_for_identity_recurses_into_nested_structures_only_on_strings() {
    let value = json!({ "name": "O\u{2019}Brien", "count": 3, "nested": ["A B", null] });
    let normalized = normalize_value_for_identity(&value);
    assert_eq!(
        normalized,
        json!({ "name": "o'brien", "count": 3, "nested": ["a b", null] })
    );
}

// ---- fold_quotes_and_trim / normalize_value_for_hash ------------------------

#[test]
fn fold_quotes_and_trim_folds_quotes_and_trims_but_does_not_lowercase_or_collapse_whitespace() {
    assert_eq!(fold_quotes_and_trim("  Bob\u{2019}s Shop  "), "Bob's Shop");
    assert_eq!(fold_quotes_and_trim("\u{201C}Quoted\u{201D}"), "\"Quoted\"");
    // Deliberately does not lowercase or collapse internal whitespace runs —
    // that's normalize_identity_text's job, not this one's.
    assert_eq!(fold_quotes_and_trim("A   B"), "A   B");
}

#[test]
fn normalize_value_for_hash_recurses_into_nested_structures_only_on_strings() {
    let value = json!({ "tag": " Bob\u{2019}s ", "count": 3, "nested": ["\u{201C}x\u{201D}", null] });
    let normalized = normalize_value_for_hash(&value);
    assert_eq!(
        normalized,
        json!({ "tag": "Bob's", "count": 3, "nested": ["\"x\"", null] })
    );
}

// ---- normalize_text (extractor_contract's prose normalizer) -----------------

#[test]
fn normalize_text_straightens_smart_quotes_and_dashes() {
    let input = "\u{2018}Hello\u{2019} \u{2014} \u{201C}world\u{201D}";
    assert_eq!(normalize_text(input), "'Hello' - \"world\"");
}

#[test]
fn normalize_text_collapses_whitespace() {
    assert_eq!(normalize_text("  a   b\t\nc  "), "a b c");
}

#[test]
fn normalize_text_applies_nfkc() {
    // U+FB01 LATIN SMALL LIGATURE FI decomposes under NFKC to "fi".
    let input = "\u{FB01}le";
    assert_eq!(normalize_text(input), "file");
}

#[test]
fn normalize_text_never_lowercases_unlike_normalize_identity_text() {
    assert_eq!(normalize_text("CAFE Corp"), "CAFE Corp");
    assert_eq!(normalize_identity_text("CAFE Corp"), "cafe corp");
}
