use super::*;
use crate::predicate::evaluate_predicate as evaluate_typed_predicate;
use serde_json::json;

fn fixture_payload() -> Value {
    json!({
        "tag": "Very Important Client",
        "status": "Open",
        "notes": "  ",
        "empty_list": [],
        "count": 42,
    })
}

/// Every operator the retired string grammar supported (`not_empty`,
/// `contains`, `equals`, `and`, `or`, `not`), plus precedence/nesting cases
/// that combine them.
const VALID_EXPRS: &[&str] = &[
    "not_empty(tag)",
    "not_empty(notes)",                // blank string -> false
    "not_empty(missing_field)",        // absent -> false
    "not_empty(empty_list)",           // empty array -> false
    "contains(tag, 'very important')", // case-insensitive
    "contains(tag, 'nope')",
    "equals(status, 'OPEN')", // case-insensitive
    "equals(status, 'closed')",
    "and(contains(tag, 'Very Important'), equals(status, 'open'))",
    "and(contains(tag, 'Very Important'), equals(status, 'closed'))",
    "or(contains(tag, 'nope'), equals(status, 'open'))",
    "or(contains(tag, 'nope'), equals(status, 'closed'))",
    "not(equals(status, 'closed'))",
    "not(not_empty(empty_list))",
    "and(contains(tag, 'Very Important'), not(equals(status, 'closed')))",
    "or(and(contains(tag, 'nope'), equals(status, 'open')), not_empty(notes))",
];

/// At least two malformed inputs — the retired parser's exact failure modes
/// (unterminated literal, wrong arity, unknown function, missing comma,
/// trailing garbage, empty input).
const MALFORMED_EXPRS: &[&str] = &[
    "",
    "contains(tag, 'unterminated",
    "and(contains(tag, 'x'))",
    "frobnicate(tag)",
    "contains(tag 'missing comma')",
    "contains(tag, 'x') trailing garbage",
];

#[test]
fn legacy_parse_differential_matches_retired_string_evaluator_for_every_operator() {
    assert!(VALID_EXPRS.len() >= 12, "battery must cover at least 12 expressions");
    let payload = fixture_payload();
    for expr in VALID_EXPRS {
        let old =
            evaluate_legacy_expr(expr, &payload).unwrap_or_else(|e| panic!("old evaluator rejected {expr:?}: {e}"));
        let typed = parse(expr).unwrap_or_else(|e| panic!("parse() rejected {expr:?}: {e}"));
        let new = evaluate_typed_predicate(&typed, &payload, None);
        assert_eq!(old, new, "legacy_parse({expr:?}) disagreed with the retired string evaluator");
    }
}

/// The failure-mode half of the differential battery: a malformed `expr`
/// must reject identically under both the retired string evaluator and the
/// new `parse()` entry point — an `Err` on one side and a silently-`true`
/// predicate on the other is exactly the spurious-firing regression this
/// migration must not introduce.
#[test]
fn legacy_parse_differential_malformed_inputs_fail_the_same_way_never_silently_true() {
    assert!(MALFORMED_EXPRS.len() >= 2, "battery must cover at least 2 malformed inputs");
    let payload = fixture_payload();
    for expr in MALFORMED_EXPRS {
        let old = evaluate_legacy_expr(expr, &payload);
        assert!(old.is_err(), "expected the retired evaluator to reject {expr:?}, got {old:?}");

        let new = parse(expr);
        assert!(new.is_err(), "expected parse() to reject {expr:?}, got {new:?}");
    }
}
