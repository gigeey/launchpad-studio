use super::*;
use serde_json::json;

// ---------------------------------------------------------------------
// infer_tier
// ---------------------------------------------------------------------

#[test]
fn json_path_is_deterministic_only_when_structured_content_and_schema_both_hold() {
    let kind = ExtractorKind::JsonPath { path: "a.b".to_string() };
    assert_eq!(infer_tier(true, true, &kind), Tier::Deterministic);
    assert_eq!(infer_tier(true, false, &kind), Tier::Probabilistic);
    assert_eq!(infer_tier(false, true, &kind), Tier::Probabilistic);
    assert_eq!(infer_tier(false, false, &kind), Tier::Probabilistic);
}

#[test]
fn regex_is_always_probabilistic() {
    let kind = ExtractorKind::Regex { pattern: "x".to_string(), capture: "0".to_string() };
    assert_eq!(infer_tier(true, true, &kind), Tier::Probabilistic);
    assert_eq!(infer_tier(false, false, &kind), Tier::Probabilistic);
}

#[test]
fn hash_is_always_change_detection_only() {
    assert_eq!(infer_tier(true, true, &ExtractorKind::Hash), Tier::ChangeDetectionOnly);
    assert_eq!(infer_tier(false, false, &ExtractorKind::Hash), Tier::ChangeDetectionOnly);
}

// ---------------------------------------------------------------------
// resolve_json_path
// ---------------------------------------------------------------------

#[test]
fn resolves_a_dotted_path_through_nested_objects() {
    let v = json!({"a": {"b": {"c": 42}}});
    assert_eq!(resolve_json_path(&v, "a.b.c"), Some(&json!(42)));
}

#[test]
fn resolves_a_numeric_array_index() {
    let v = json!({"a": {"b": [10, 20, 30]}});
    assert_eq!(resolve_json_path(&v, "a.b[1]"), Some(&json!(20)));
}

#[test]
fn resolves_a_path_mixing_dots_and_indices() {
    let v = json!({"a": [ {"c": 1}, {"c": 2} ]});
    assert_eq!(resolve_json_path(&v, "a[1].c"), Some(&json!(2)));
    assert_eq!(resolve_json_path(&v, "a[0].c"), Some(&json!(1)));
}

#[test]
fn missing_key_resolves_to_none() {
    let v = json!({"a": {"b": 1}});
    assert_eq!(resolve_json_path(&v, "a.missing"), None);
}

#[test]
fn missing_index_resolves_to_none() {
    let v = json!({"a": [1, 2]});
    assert_eq!(resolve_json_path(&v, "a[5]"), None);
}

#[test]
fn empty_path_resolves_to_root() {
    let v = json!({"a": 1});
    assert_eq!(resolve_json_path(&v, ""), Some(&v));
}

#[test]
fn indexing_into_a_non_array_resolves_to_none() {
    let v = json!({"a": {"b": 1}});
    assert_eq!(resolve_json_path(&v, "a[0]"), None);
}

#[test]
fn keying_into_a_non_object_resolves_to_none() {
    let v = json!({"a": [1, 2]});
    assert_eq!(resolve_json_path(&v, "a.b"), None);
}

#[test]
fn malformed_paths_resolve_to_none_without_panicking() {
    let v = json!({"a": 1});
    assert_eq!(resolve_json_path(&v, "a["), None); // unterminated bracket
    assert_eq!(resolve_json_path(&v, "a[x]"), None); // non-numeric index
    assert_eq!(resolve_json_path(&v, "a.."), None); // empty segment
}

// ---------------------------------------------------------------------
// enumerate_paths — the shape-report primitive
// ---------------------------------------------------------------------

#[test]
fn enumerates_leaf_paths_with_type_only() {
    let v = json!({
        "name": "irrelevant",
        "count": 3,
        "active": true,
        "missing": null,
        "tags": ["a", "b"],
        "nested": {"inner": 1.5},
    });
    let paths = enumerate_paths(&v, DEFAULT_MAX_DEPTH, DEFAULT_MAX_PATHS);
    let find = |p: &str| paths.iter().find(|info| info.path == p).map(|info| info.value_type.as_str());

    assert_eq!(find("name"), Some("string"));
    assert_eq!(find("count"), Some("number"));
    assert_eq!(find("active"), Some("bool"));
    assert_eq!(find("missing"), Some("null"));
    assert_eq!(find("tags"), Some("array[2]"));
    assert_eq!(find("tags[0]"), Some("string"));
    assert_eq!(find("tags[1]"), Some("string"));
    assert_eq!(find("nested"), Some("object"));
    assert_eq!(find("nested.inner"), Some("number"));
}

#[test]
fn enumerate_paths_never_leaks_a_value_even_when_the_value_looks_like_a_path() {
    let v = json!({
        "field_one": "a.b[0].c",
        "field_two": "sk_live_super_secret_marker_should_never_appear_9f8e7d",
        "nested": {"field_three": "another.dotted.value"},
    });
    let paths = enumerate_paths(&v, DEFAULT_MAX_DEPTH, DEFAULT_MAX_PATHS);
    let serialized = serde_json::to_string(&paths).expect("serialize path report");

    assert!(!serialized.contains("a.b[0].c"));
    assert!(!serialized.contains("sk_live_super_secret_marker_should_never_appear_9f8e7d"));
    assert!(!serialized.contains("another.dotted.value"));
    // The report is not empty — it does describe the shape, just not the values.
    assert!(serialized.contains("field_one"));
    assert!(serialized.contains("\"string\""));
}

#[test]
fn enumerate_paths_respects_max_depth() {
    let v = json!({"a": {"b": {"c": {"d": 1}}}});
    let paths = enumerate_paths(&v, 2, DEFAULT_MAX_PATHS);
    let has = |p: &str| paths.iter().any(|info| info.path == p);
    assert!(has("a"));
    assert!(has("a.b"));
    // Depth 2 stops descending before reaching "a.b.c" or deeper.
    assert!(!has("a.b.c"));
    assert!(!has("a.b.c.d"));
}

#[test]
fn enumerate_paths_respects_max_paths() {
    let v = json!({"a": 1, "b": 2, "c": 3, "d": 4, "e": 5});
    let paths = enumerate_paths(&v, DEFAULT_MAX_DEPTH, 2);
    assert_eq!(paths.len(), 2);
}

// ---------------------------------------------------------------------
// normalize_text
// ---------------------------------------------------------------------

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

// ---------------------------------------------------------------------
// Predicate — every variant
// ---------------------------------------------------------------------

#[test]
fn predicate_exists() {
    let v = json!({"a": 1, "b": null});
    assert!(evaluate_predicate(&Predicate::Exists { path: "a".to_string() }, &v, None));
    assert!(!evaluate_predicate(&Predicate::Exists { path: "b".to_string() }, &v, None));
    assert!(!evaluate_predicate(&Predicate::Exists { path: "missing".to_string() }, &v, None));
}

#[test]
fn predicate_equals_and_not_equals() {
    let v = json!({"status": "open"});
    assert!(evaluate_predicate(
        &Predicate::Equals { path: "status".to_string(), value: json!("open") },
        &v,
        None
    ));
    assert!(!evaluate_predicate(
        &Predicate::Equals { path: "status".to_string(), value: json!("closed") },
        &v,
        None
    ));
    assert!(evaluate_predicate(
        &Predicate::NotEquals { path: "status".to_string(), value: json!("closed") },
        &v,
        None
    ));
    assert!(!evaluate_predicate(
        &Predicate::NotEquals { path: "status".to_string(), value: json!("open") },
        &v,
        None
    ));
}

#[test]
fn predicate_contains() {
    let v = json!({"title": "Quarterly Report Draft"});
    assert!(evaluate_predicate(
        &Predicate::Contains { path: "title".to_string(), substring: "Report".to_string() },
        &v,
        None
    ));
    assert!(!evaluate_predicate(
        &Predicate::Contains { path: "title".to_string(), substring: "Invoice".to_string() },
        &v,
        None
    ));
    // Non-string path never matches, never errors.
    let numeric = json!({"title": 5});
    assert!(!evaluate_predicate(
        &Predicate::Contains { path: "title".to_string(), substring: "5".to_string() },
        &numeric,
        None
    ));
}

#[test]
fn predicate_greater_than_and_less_than() {
    let v = json!({"count": 10});
    assert!(evaluate_predicate(&Predicate::GreaterThan { path: "count".to_string(), number: 5.0 }, &v, None));
    assert!(!evaluate_predicate(&Predicate::GreaterThan { path: "count".to_string(), number: 10.0 }, &v, None));
    assert!(evaluate_predicate(&Predicate::LessThan { path: "count".to_string(), number: 20.0 }, &v, None));
    assert!(!evaluate_predicate(&Predicate::LessThan { path: "count".to_string(), number: 10.0 }, &v, None));

    // Non-numeric path never matches, never errors.
    let non_numeric = json!({"count": "ten"});
    assert!(!evaluate_predicate(&Predicate::GreaterThan { path: "count".to_string(), number: 0.0 }, &non_numeric, None));
}

#[test]
fn predicate_changed() {
    let current = json!({"status": "open"});
    let same = json!({"status": "open"});
    let different = json!({"status": "closed"});
    let predicate = Predicate::Changed { path: "status".to_string() };

    assert!(evaluate_predicate(&predicate, &current, None), "no previous observation counts as changed");
    assert!(!evaluate_predicate(&predicate, &current, Some(&same)), "identical values are not a change");
    assert!(evaluate_predicate(&predicate, &current, Some(&different)), "differing values are a change");
}

#[test]
fn predicate_and_or_not() {
    let v = json!({"a": 1, "b": 2});
    let a_exists = Predicate::Exists { path: "a".to_string() };
    let c_exists = Predicate::Exists { path: "c".to_string() };

    assert!(evaluate_predicate(&Predicate::And(vec![a_exists.clone()]), &v, None));
    assert!(!evaluate_predicate(&Predicate::And(vec![a_exists.clone(), c_exists.clone()]), &v, None));
    assert!(evaluate_predicate(&Predicate::And(vec![]), &v, None), "empty And is vacuously true");

    assert!(evaluate_predicate(&Predicate::Or(vec![a_exists.clone(), c_exists.clone()]), &v, None));
    assert!(!evaluate_predicate(&Predicate::Or(vec![c_exists.clone()]), &v, None));
    assert!(!evaluate_predicate(&Predicate::Or(vec![]), &v, None), "empty Or is vacuously false");

    assert!(!evaluate_predicate(&Predicate::Not(Box::new(a_exists.clone())), &v, None));
    assert!(evaluate_predicate(&Predicate::Not(Box::new(c_exists.clone())), &v, None));
}

// ---------------------------------------------------------------------
// Normalization scoping: JsonPath tier is unnormalized; Regex/Hash are not.
// ---------------------------------------------------------------------

#[test]
fn json_path_predicate_comparison_is_unnormalized() {
    // Curly apostrophe in the observed value; the contract author compares
    // against a straight apostrophe. If Equals normalized first, these would
    // match — the whole point of the scoping rule is that they must not.
    let v = json!({"note": "it\u{2019}s fine"});
    let predicate = Predicate::Equals { path: "note".to_string(), value: json!("it's fine") };
    assert!(!evaluate_predicate(&predicate, &v, None));

    // Sanity: normalize_text alone *would* fold these to the same string —
    // proving the mismatch above is because Equals never calls it, not
    // because the two strings are actually different once normalized.
    assert_eq!(normalize_text("it\u{2019}s fine"), normalize_text("it's fine"));
}

#[test]
fn regex_selection_matches_against_normalized_text() {
    let contract = ExtractionPlan {
        selector: Selector {
            kind: ExtractorKind::Regex { pattern: String::new(), capture: "0".to_string() },
            expr: "'there'".to_string(),
        },
        identity: ExtractorKind::Hash,
        predicate: Predicate::Exists { path: String::new() },
    };
    // Curly quotes and an em dash in the source text; the pattern is plain ASCII.
    let text = "Hi \u{2018}there\u{2019} \u{2014} friend";
    let resolution = resolve(&contract, None, Some(text)).expect("regex should match after normalization");
    assert_eq!(resolution.items.len(), 1);
    assert_eq!(resolution.items[0].value, json!("'there'"));
}

#[test]
fn hash_identity_is_stable_across_cosmetic_text_variance() {
    let contract = ExtractionPlan {
        selector: Selector {
            kind: ExtractorKind::Hash,
            expr: String::new(),
        },
        identity: ExtractorKind::Hash,
        predicate: Predicate::Exists { path: String::new() },
    };
    let a = resolve(&contract, None, Some("Hello \u{2018}world\u{2019}")).unwrap();
    let b = resolve(&contract, None, Some("Hello 'world'")).unwrap();
    assert_eq!(a.items[0].id, b.items[0].id, "cosmetic quote variance must not change the Hash-tier identity");
}

// ---------------------------------------------------------------------
// resolve / BindError
// ---------------------------------------------------------------------

fn json_path_contract(path: &str) -> ExtractionPlan {
    ExtractionPlan {
        selector: Selector { kind: ExtractorKind::JsonPath { path: String::new() }, expr: path.to_string() },
        identity: ExtractorKind::JsonPath { path: "id".to_string() },
        predicate: Predicate::Exists { path: "id".to_string() },
    }
}

#[test]
fn resolve_yields_one_item_per_array_element() {
    let structured = json!({"items": [{"id": "a"}, {"id": "b"}, {"id": "c"}]});
    let contract = json_path_contract("items");
    let resolution = resolve(&contract, Some(&structured), None).expect("path resolves");
    assert_eq!(resolution.items.len(), 3);
    assert_eq!(resolution.items[0].id, canonical_json(&json!("a")));
    assert!(resolution.items.iter().all(|i| i.matched));
}

/// The literal envelope a live Notion `notion-query-data-sources` call
/// returns (verified against a real workspace, not synthesized): a
/// top-level `results` array of flat row objects, alongside sibling
/// metadata fields (`has_more`, `data_source_ids`) that are not the data
/// itself. A selector of `"results"` must resolve to exactly the two rows,
/// each keeping its own four fields untouched.
#[test]
fn resolve_selects_results_array_from_a_notion_query_data_sources_envelope() {
    let structured = json!({
        "results": [
            {
                "url": "https://app.notion.com/a1b2c3d4e5f60718293a4b5c6d7e8f90",
                "Company": "Rose's Roses",
                "First name": "Rose",
                "Last Name": "Flamingo"
            },
            {
                "url": "https://app.notion.com/0f9e8d7c6b5a49382716f5e4d3c2b1a0",
                "Company": "Second Client Co",
                "First name": "Second",
                "Last Name": "Client"
            }
        ],
        "has_more": false,
        "data_source_ids": ["collection://11112222-3333-4444-5555-666677778888"]
    });
    let contract = json_path_contract("results");
    let resolution = resolve(&contract, Some(&structured), None).expect("results path resolves");

    assert_eq!(resolution.items.len(), 2, "both rows in `results` must be selected, nothing from the siblings");
    for item in &resolution.items {
        let obj = item.value.as_object().expect("each item is a flat row object");
        assert_eq!(obj.len(), 4, "each row keeps exactly its own four fields");
        for field in ["url", "Company", "First name", "Last Name"] {
            assert!(obj.contains_key(field), "row is missing expected field {field:?}: {obj:?}");
        }
    }
    assert_eq!(
        resolution.items[0].value["Company"], json!("Rose's Roses"),
        "field values must pass through unchanged"
    );
}

#[test]
fn resolve_yields_a_single_item_for_a_scalar_or_object_selector() {
    let structured = json!({"item": {"id": "solo"}});
    let contract = json_path_contract("item");
    let resolution = resolve(&contract, Some(&structured), None).expect("path resolves");
    assert_eq!(resolution.items.len(), 1);
}

#[test]
fn resolve_reports_structured_path_not_found_with_available_paths() {
    let structured = json!({"a": {"b": 1}, "c": [1, 2, 3]});
    let contract = json_path_contract("a.missing");
    let err = resolve(&contract, Some(&structured), None).unwrap_err();
    match err {
        BindError::StructuredPathNotFound { path, available_paths } => {
            assert_eq!(path, "a.missing");
            assert!(!available_paths.is_empty());
            assert!(available_paths.iter().any(|p| p.path == "a.b"));
            assert!(available_paths.iter().any(|p| p.path == "c"));
        }
        other => panic!("expected StructuredPathNotFound, got {other:?}"),
    }
}

#[test]
fn resolve_regex_with_zero_matches_is_ok_with_empty_items_not_an_error() {
    // A quiet poll — the pattern is perfectly valid, it just doesn't match
    // anything in this particular text. That's the ordinary case for a
    // watch, not a contract bug, so it must not be `Err`.
    let contract = ExtractionPlan {
        selector: Selector {
            kind: ExtractorKind::Regex { pattern: String::new(), capture: "0".to_string() },
            expr: "NEVER_PRESENT_PATTERN".to_string(),
        },
        identity: ExtractorKind::Hash,
        predicate: Predicate::Exists { path: String::new() },
    };
    let resolution = resolve(&contract, None, Some("nothing to see here"))
        .expect("a valid pattern that matches nothing must resolve Ok, not Err");
    assert!(resolution.items.is_empty());
}

#[test]
fn resolve_regex_with_multiple_matches_returns_one_item_per_match() {
    let contract = ExtractionPlan {
        selector: Selector {
            kind: ExtractorKind::Regex { pattern: String::new(), capture: "0".to_string() },
            expr: "id-\\d+".to_string(),
        },
        identity: ExtractorKind::Hash,
        predicate: Predicate::Exists { path: String::new() },
    };
    let text = "id-1 and id-2 and id-3";
    let resolution = resolve(&contract, None, Some(text)).expect("pattern matches");
    assert_eq!(resolution.items.len(), 3);
    assert_eq!(resolution.items[0].value, json!("id-1"));
    assert_eq!(resolution.items[1].value, json!("id-2"));
    assert_eq!(resolution.items[2].value, json!("id-3"));
}

#[test]
fn resolve_reports_no_content_supplied() {
    let contract = json_path_contract("a");
    assert_eq!(resolve(&contract, None, None).unwrap_err(), BindError::NoContentSupplied);

    let regex_contract = ExtractionPlan {
        selector: Selector {
            kind: ExtractorKind::Regex { pattern: String::new(), capture: "0".to_string() },
            expr: "x".to_string(),
        },
        identity: ExtractorKind::Hash,
        predicate: Predicate::Exists { path: String::new() },
    };
    assert_eq!(resolve(&regex_contract, None, None).unwrap_err(), BindError::NoContentSupplied);
}

#[test]
fn resolve_reports_invalid_regex_pattern() {
    let contract = ExtractionPlan {
        selector: Selector {
            kind: ExtractorKind::Regex { pattern: String::new(), capture: "0".to_string() },
            expr: "(unterminated".to_string(),
        },
        identity: ExtractorKind::Hash,
        predicate: Predicate::Exists { path: String::new() },
    };
    let err = resolve(&contract, None, Some("some text")).unwrap_err();
    assert!(matches!(err, BindError::InvalidPattern { .. }));
}

#[test]
fn resolve_applies_identity_and_predicate_per_item() {
    let structured = json!({"items": [{"id": "a", "flag": true}, {"id": "b", "flag": false}]});
    let contract = ExtractionPlan {
        selector: Selector { kind: ExtractorKind::JsonPath { path: String::new() }, expr: "items".to_string() },
        identity: ExtractorKind::JsonPath { path: "id".to_string() },
        predicate: Predicate::Equals { path: "flag".to_string(), value: json!(true) },
    };
    let resolution = resolve(&contract, Some(&structured), None).unwrap();
    assert_eq!(resolution.items.len(), 2);
    assert!(resolution.items[0].matched);
    assert!(!resolution.items[1].matched);
}

// ---------------------------------------------------------------------
// Serde round trip
// ---------------------------------------------------------------------

#[test]
fn extractor_kind_serializes_with_a_type_tag() {
    let json_path = ExtractorKind::JsonPath { path: "a.b".to_string() };
    let s = serde_json::to_string(&json_path).unwrap();
    assert!(s.contains("\"type\":\"JsonPath\""), "got: {s}");

    let hash = ExtractorKind::Hash;
    let s = serde_json::to_string(&hash).unwrap();
    assert!(s.contains("\"type\":\"Hash\""), "got: {s}");
}

#[test]
fn full_contract_round_trips_through_json() {
    let contract = ExtractionPlan {
        selector: Selector {
            kind: ExtractorKind::JsonPath { path: String::new() },
            expr: "results".to_string(),
        },
        identity: ExtractorKind::Regex { pattern: "id-(\\d+)".to_string(), capture: "1".to_string() },
        predicate: Predicate::And(vec![
            Predicate::Exists { path: "title".to_string() },
            Predicate::Or(vec![
                Predicate::GreaterThan { path: "score".to_string(), number: 3.0 },
                Predicate::Not(Box::new(Predicate::Contains {
                    path: "title".to_string(),
                    substring: "draft".to_string(),
                })),
            ]),
            Predicate::Changed { path: "status".to_string() },
        ]),
    };

    let json = serde_json::to_string(&contract).expect("serialize");
    let round_tripped: ExtractionPlan = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(contract, round_tripped);
}
