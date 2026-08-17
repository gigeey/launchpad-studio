use super::*;
use serde_json::json;

// `Exists`/`Equals`/`NotEquals`/`Contains`/`GreaterThan`/`LessThan`/`Changed`/
// `And`/`Or`/`Not`/`resolve_json_path` are covered by
// `crate::extractor_contract`'s test suite (this module re-exports both
// `Predicate` and `resolve_json_path` from there unchanged). This file only
// covers the three variants added here for `watch_contract`'s legacy-grammar
// fidelity, which have no equivalent in `extractor_contract`.

#[test]
fn not_empty_true_for_non_blank_scalars_and_non_empty_containers() {
    let v = json!({
        "s": "hello",
        "arr": [1],
        "obj": { "k": 1 },
        "n": 3,
        "b": false,
    });
    for path in ["s", "arr", "obj", "n", "b"] {
        assert!(
            evaluate_predicate(&Predicate::NotEmpty { path: path.to_string() }, &v, None),
            "expected NotEmpty to hold for {path}"
        );
    }
}

#[test]
fn not_empty_false_for_blank_empty_null_or_missing() {
    let v = json!({
        "blank": "   ",
        "empty_str": "",
        "empty_arr": [],
        "empty_obj": {},
        "null_field": null,
    });
    for path in ["blank", "empty_str", "empty_arr", "empty_obj", "null_field", "missing"] {
        assert!(
            !evaluate_predicate(&Predicate::NotEmpty { path: path.to_string() }, &v, None),
            "expected NotEmpty to fail for {path}"
        );
    }
}

#[test]
fn contains_ci_matches_regardless_of_case() {
    let v = json!({ "tag": "Very Important Client" });
    assert!(evaluate_predicate(
        &Predicate::ContainsCi { path: "tag".to_string(), literal: "very important".to_string() },
        &v,
        None
    ));
    assert!(!evaluate_predicate(
        &Predicate::ContainsCi { path: "tag".to_string(), literal: "urgent".to_string() },
        &v,
        None
    ));
}

#[test]
fn contains_ci_missing_field_is_false_not_error() {
    let v = json!({ "other": "x" });
    assert!(!evaluate_predicate(
        &Predicate::ContainsCi { path: "tag".to_string(), literal: "x".to_string() },
        &v,
        None
    ));
}

#[test]
fn contains_ci_coerces_non_string_values_via_display() {
    let v = json!({ "count": 42 });
    assert!(evaluate_predicate(
        &Predicate::ContainsCi { path: "count".to_string(), literal: "4".to_string() },
        &v,
        None
    ));
}

#[test]
fn equals_ci_matches_regardless_of_case() {
    let v = json!({ "status": "OPEN" });
    assert!(evaluate_predicate(
        &Predicate::EqualsCi { path: "status".to_string(), literal: "open".to_string() },
        &v,
        None
    ));
    assert!(!evaluate_predicate(
        &Predicate::EqualsCi { path: "status".to_string(), literal: "closed".to_string() },
        &v,
        None
    ));
}

#[test]
fn equals_ci_missing_field_is_false() {
    let v = json!({});
    assert!(!evaluate_predicate(
        &Predicate::EqualsCi { path: "status".to_string(), literal: "open".to_string() },
        &v,
        None
    ));
}

#[test]
fn new_variants_serde_round_trip() {
    let predicates = vec![
        Predicate::NotEmpty { path: "a".to_string() },
        Predicate::ContainsCi { path: "a".to_string(), literal: "x".to_string() },
        Predicate::EqualsCi { path: "a".to_string(), literal: "x".to_string() },
    ];
    for predicate in predicates {
        let json = serde_json::to_string(&predicate).expect("serialize");
        let round_tripped: Predicate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(predicate, round_tripped);
    }
}

// ---- paths_matching ---------------------------------------------------------

#[test]
fn paths_matching_all_leaves_collects_every_referenced_path_through_combinators() {
    let predicate = Predicate::And(vec![
        Predicate::NotEmpty { path: "a".to_string() },
        Predicate::Or(vec![
            Predicate::EqualsCi { path: "b".to_string(), literal: "x".to_string() },
            Predicate::Not(Box::new(Predicate::ContainsCi { path: "c".to_string(), literal: "y".to_string() })),
        ]),
    ]);
    let mut paths = predicate.paths_matching(|_| true);
    paths.sort();
    assert_eq!(paths, vec!["a", "b", "c"], "a `|_| true` filter must behave as a generic referenced-fields collector");
}

#[test]
fn paths_matching_filters_to_the_requested_leaf_variant() {
    let predicate = Predicate::And(vec![
        Predicate::NotEmpty { path: "summary".to_string() },
        Predicate::Not(Box::new(Predicate::NotEmpty { path: "notes".to_string() })),
        Predicate::EqualsCi { path: "status".to_string(), literal: "open".to_string() },
    ]);
    let mut not_empty_paths = predicate.paths_matching(|p| matches!(p, Predicate::NotEmpty { .. }));
    not_empty_paths.sort();
    assert_eq!(not_empty_paths, vec!["notes", "summary"], "non-NotEmpty leaves must be excluded");
}

#[test]
fn paths_matching_returns_empty_when_nothing_matches() {
    let predicate = Predicate::NotEmpty { path: "a".to_string() };
    assert!(predicate.paths_matching(|p| matches!(p, Predicate::Changed { .. })).is_empty());
}

// ---- to_expr / legacy grammar round-trip ---------------------------------

/// The property that makes the contract explainer's "show raw expression"
/// trustworthy as a diagnostic: what it displays parses back to exactly the
/// predicate the runtime evaluates. Anything less and the panel is showing a
/// plausible-looking lie.
#[test]
fn to_expr_round_trips_through_legacy_parse() {
    let cases = vec![
        // leaves
        Predicate::NotEmpty { path: "summary".to_string() },
        Predicate::ContainsCi { path: "tag".to_string(), literal: "Very Important".to_string() },
        Predicate::EqualsCi { path: "status".to_string(), literal: "open".to_string() },
        // identifier shapes the grammar accepts
        Predicate::NotEmpty { path: "client_email".to_string() },
        Predicate::NotEmpty { path: "field2".to_string() },
        // combinators, at the exactly-binary arity the grammar has
        Predicate::Not(Box::new(Predicate::NotEmpty { path: "summary".to_string() })),
        Predicate::And(vec![
            Predicate::ContainsCi { path: "tag".to_string(), literal: "urgent".to_string() },
            Predicate::EqualsCi { path: "status".to_string(), literal: "open".to_string() },
        ]),
        Predicate::Or(vec![
            Predicate::NotEmpty { path: "summary".to_string() },
            Predicate::Not(Box::new(Predicate::EqualsCi { path: "status".to_string(), literal: "done".to_string() })),
        ]),
        // nesting on both sides
        Predicate::And(vec![
            Predicate::Or(vec![
                Predicate::NotEmpty { path: "a".to_string() },
                Predicate::NotEmpty { path: "b".to_string() },
            ]),
            Predicate::Not(Box::new(Predicate::ContainsCi { path: "c".to_string(), literal: "x".to_string() })),
        ]),
        // literal escaping: quote, backslash, both, and grammar punctuation
        Predicate::ContainsCi { path: "note".to_string(), literal: "it's urgent".to_string() },
        Predicate::ContainsCi { path: "note".to_string(), literal: r"a\b".to_string() },
        Predicate::ContainsCi { path: "note".to_string(), literal: r"trailing\".to_string() },
        Predicate::EqualsCi { path: "note".to_string(), literal: r"don't, ok\)".to_string() },
        Predicate::ContainsCi { path: "note".to_string(), literal: "and(a, b)".to_string() },
        // empty literal is still a well-formed string in the grammar
        Predicate::EqualsCi { path: "note".to_string(), literal: String::new() },
    ];

    for predicate in cases {
        let rendered = predicate.to_expr().unwrap_or_else(|| panic!("expected renderable: {predicate:?}"));
        let reparsed = crate::watch_contract::legacy_expr::parse(&rendered)
            .unwrap_or_else(|e| panic!("rendered {rendered:?} failed to parse back: {e}"));
        assert_eq!(reparsed, predicate, "round-trip changed the predicate (via {rendered:?})");
    }
}

#[test]
fn to_expr_renders_the_expected_grammar_text() {
    assert_eq!(
        Predicate::ContainsCi { path: "tag".to_string(), literal: "Very Important".to_string() }.to_expr(),
        Some("contains(tag, 'Very Important')".to_string())
    );
    assert_eq!(
        Predicate::NotEmpty { path: "summary".to_string() }.to_expr(),
        Some("not_empty(summary)".to_string())
    );
    assert_eq!(
        Predicate::And(vec![
            Predicate::NotEmpty { path: "a".to_string() },
            Predicate::EqualsCi { path: "b".to_string(), literal: "x".to_string() },
        ])
        .to_expr(),
        Some("and(not_empty(a), equals(b, 'x'))".to_string())
    );
}

/// Variants the typed enum has and the string grammar never did must render
/// as `None`, so the UI says "no expression available" instead of inventing
/// a spelling the parser would reject.
#[test]
fn to_expr_is_none_for_predicates_the_grammar_cannot_express() {
    let unrepresentable = vec![
        Predicate::Exists { path: "a".to_string() },
        Predicate::Equals { path: "a".to_string(), value: serde_json::json!("x") },
        Predicate::NotEquals { path: "a".to_string(), value: serde_json::json!(1) },
        Predicate::Contains { path: "a".to_string(), substring: "x".to_string() },
        Predicate::GreaterThan { path: "a".to_string(), number: 1.0 },
        Predicate::LessThan { path: "a".to_string(), number: 1.0 },
        Predicate::Changed { path: "a".to_string() },
    ];
    for predicate in unrepresentable {
        assert_eq!(predicate.to_expr(), None, "expected None for {predicate:?}");
    }
}

#[test]
fn to_expr_is_none_for_paths_and_arities_outside_the_grammar() {
    // Dotted/indexed paths resolve fine at runtime but have no spelling as a
    // grammar identifier.
    assert_eq!(Predicate::NotEmpty { path: "a.b".to_string() }.to_expr(), None);
    assert_eq!(Predicate::NotEmpty { path: "a[0]".to_string() }.to_expr(), None);
    assert_eq!(Predicate::NotEmpty { path: "has space".to_string() }.to_expr(), None);
    assert_eq!(Predicate::NotEmpty { path: "has-dash".to_string() }.to_expr(), None);
    assert_eq!(Predicate::NotEmpty { path: String::new() }.to_expr(), None);
    // Vacuous combinators have no constant in the grammar.
    assert_eq!(Predicate::And(vec![]).to_expr(), None);
    assert_eq!(Predicate::Or(vec![]).to_expr(), None);
    // Unrepresentability propagates out through combinators rather than
    // rendering a partial expression.
    assert_eq!(Predicate::Not(Box::new(Predicate::Changed { path: "a".to_string() })).to_expr(), None);
    assert_eq!(
        Predicate::And(vec![
            Predicate::NotEmpty { path: "a".to_string() },
            Predicate::GreaterThan { path: "b".to_string(), number: 2.0 },
        ])
        .to_expr(),
        None
    );
    assert_eq!(
        Predicate::Or(vec![
            Predicate::NotEmpty { path: "a".to_string() },
            Predicate::NotEmpty { path: "a.b".to_string() },
        ])
        .to_expr(),
        None
    );
}

/// n-ary `And`/`Or` right-fold into the grammar's binary form, so they do not
/// round-trip *structurally*. Pin the weaker guarantee that actually matters
/// for a diagnostic: the rendered text evaluates identically to the predicate
/// the runtime is running, for every combination of the relevant inputs.
#[test]
fn nary_combinators_fold_to_evaluation_identical_binary_form() {
    let leaf = |name: &str| Predicate::NotEmpty { path: name.to_string() };
    let cases = vec![
        Predicate::And(vec![leaf("a"), leaf("b"), leaf("c")]),
        Predicate::Or(vec![leaf("a"), leaf("b"), leaf("c")]),
        Predicate::And(vec![leaf("a")]),
        Predicate::Or(vec![leaf("a")]),
        Predicate::And(vec![leaf("a"), leaf("b"), leaf("c"), leaf("d")]),
        Predicate::Or(vec![Predicate::And(vec![leaf("a"), leaf("b"), leaf("c")]), leaf("d")]),
    ];

    for predicate in cases {
        let rendered = predicate.to_expr().unwrap_or_else(|| panic!("expected renderable: {predicate:?}"));
        let reparsed = crate::watch_contract::legacy_expr::parse(&rendered)
            .unwrap_or_else(|e| panic!("rendered {rendered:?} failed to parse back: {e}"));
        // Exhaustive truth table over the four leaves.
        for bits in 0u8..16 {
            let payload = serde_json::json!({
                "a": if bits & 1 != 0 { "x" } else { "" },
                "b": if bits & 2 != 0 { "x" } else { "" },
                "c": if bits & 4 != 0 { "x" } else { "" },
                "d": if bits & 8 != 0 { "x" } else { "" },
            });
            assert_eq!(
                evaluate_predicate(&predicate, &payload, None),
                evaluate_predicate(&reparsed, &payload, None),
                "folded form {rendered:?} disagreed with {predicate:?} on {payload}"
            );
        }
    }
}

/// The `Ci` variants are case-insensitive; rendering must not quietly produce
/// a case-sensitive spelling (there isn't one in this grammar, but pin it).
#[test]
fn rendered_ci_variants_keep_case_insensitive_evaluation() {
    let predicate = Predicate::ContainsCi { path: "tag".to_string(), literal: "IMPORTANT".to_string() };
    let reparsed = crate::watch_contract::legacy_expr::parse(&predicate.to_expr().expect("renderable"))
        .expect("parses back");
    let payload = serde_json::json!({ "tag": "very important" });
    assert!(evaluate_predicate(&reparsed, &payload, None));
    assert_eq!(reparsed, predicate);
}
