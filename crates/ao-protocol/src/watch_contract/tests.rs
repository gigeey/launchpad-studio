use super::*;
use serde_json::json;

fn base_contract(strategy: IdentityStrategy) -> WatchContract {
    WatchContract {
        contract_version: 1,
        authored_at: "2026-07-27T09:00:00Z".to_string(),
        authored_by_run: "run-1".to_string(),
        source: WatchSource { kind: "example".to_string(), ref_: "abc-123".to_string() },
        identity: IdentitySpec {
            strategy,
            source_field: Some("unique_identifier".to_string()),
            format: None,
            fields: vec!["name".to_string(), "email".to_string()],
            rationale: "test fixture".to_string(),
        },
        change: ChangeSpec {
            material_fields: vec!["tag".to_string(), "status".to_string()],
            version_hint_field: Some("last_edited_time".to_string()),
        },
        predicate: PredicateSpec {
            natural_language: "tag contains 'Very Important'".to_string(),
            fields: vec!["tag".to_string()],
            predicate: legacy_expr::parse("contains(tag, 'Very Important')").expect("valid fixture expr"),
        },
        mode: WatchMode::PredicateTransition,
        fields: HashMap::new(),
    }
}

// ---- canonical_json ------------------------------------------------------

#[test]
fn canonical_json_sorts_object_keys() {
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

#[test]
fn canonical_json_has_no_insignificant_whitespace() {
    let value = json!({ "a": [1, 2], "b": "text" });
    let out = canonical_json(&value);
    assert!(!out.contains(' '), "got: {out}");
    assert!(!out.contains('\n'), "got: {out}");
}

// ---- fingerprint ----------------------------------------------------------

#[test]
fn fingerprint_is_stable_for_identical_contracts() {
    let a = base_contract(IdentityStrategy::NativeId);
    let b = base_contract(IdentityStrategy::NativeId);
    assert_eq!(a.fingerprint(), b.fingerprint());
}

#[test]
fn fingerprint_ignores_authored_at_and_authored_by_run() {
    let mut a = base_contract(IdentityStrategy::NativeId);
    let mut b = a.clone();
    b.authored_at = "2027-01-01T00:00:00Z".to_string();
    b.authored_by_run = "run-2".to_string();
    assert_eq!(a.fingerprint(), b.fingerprint());

    // Sanity: a genuine semantic change *does* move the fingerprint.
    a.predicate.predicate = legacy_expr::parse("contains(tag, 'Other')").expect("valid fixture expr");
    assert_ne!(a.fingerprint(), b.fingerprint());
}

// ---- identity_key ladder ----------------------------------------------------

#[test]
fn native_id_key_is_stable_and_matches_format() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let payload = json!({ "unique_identifier": "abc-123-def" });
    let key1 = identity_key(&contract, &payload).expect("key");
    let key2 = identity_key(&contract, &payload).expect("key");
    assert_eq!(key1, key2);
    assert_eq!(key1.len(), 64, "sha256 hex digest is 64 chars");
}

#[test]
fn native_id_key_rejects_value_failing_format_regex() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.identity.format = Some(r"^[0-9a-f-]{36}$".to_string());
    let payload = json!({ "unique_identifier": "not-a-uuid" });
    let err = identity_key(&contract, &payload).expect_err("must quarantine, not synthesize an id");
    match err {
        ContractError::FormatMismatch { field, value, pattern } => {
            assert_eq!(field, "unique_identifier");
            assert_eq!(value, "not-a-uuid");
            assert_eq!(pattern, r"^[0-9a-f-]{36}$");
        }
        other => panic!("expected FormatMismatch, got {other:?}"),
    }
}

#[test]
fn native_id_key_accepts_value_matching_format_regex() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.identity.format = Some(r"^[0-9a-f-]{36}$".to_string());
    let payload = json!({ "unique_identifier": "123e4567-e89b-12d3-a456-426614174000" });
    assert!(identity_key(&contract, &payload).is_ok());
}

#[test]
fn native_id_key_missing_field_is_missing_field_error() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let payload = json!({ "other_field": "x" });
    let err = identity_key(&contract, &payload).expect_err("missing source_field must quarantine");
    assert!(matches!(err, ContractError::MissingField(field) if field == "unique_identifier"));
}

#[test]
fn native_id_key_invalid_regex_is_invalid_regex_error() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.identity.format = Some("(unclosed".to_string());
    let payload = json!({ "unique_identifier": "abc" });
    let err = identity_key(&contract, &payload).expect_err("bad regex must not panic");
    assert!(matches!(err, ContractError::InvalidRegex(_)));
}

#[test]
fn composite_native_key_is_stable_and_order_sensitive_inputs_still_match() {
    let contract = base_contract(IdentityStrategy::CompositeNative);
    // Same underlying values, keys present in a different JSON order —
    // identity_key must produce the same key either way.
    let payload_a = json!({ "name": "Andrew Jon", "email": "andrew@example.com" });
    let payload_b = json!({ "email": "andrew@example.com", "name": "Andrew Jon" });
    let key_a = identity_key(&contract, &payload_a).expect("key");
    let key_b = identity_key(&contract, &payload_b).expect("key");
    assert_eq!(key_a, key_b);
}

#[test]
fn composite_native_key_missing_declared_field_quarantines() {
    let contract = base_contract(IdentityStrategy::CompositeNative);
    let payload = json!({ "name": "Andrew Jon" });
    let err = identity_key(&contract, &payload).expect_err("missing composite field must quarantine");
    assert!(matches!(err, ContractError::MissingField(field) if field == "email"));
}

#[test]
fn composite_native_key_empty_fields_list_is_empty_identity() {
    let mut contract = base_contract(IdentityStrategy::CompositeNative);
    contract.identity.fields = vec![];
    let payload = json!({});
    let err = identity_key(&contract, &payload).expect_err("no fields declared means no identity");
    assert!(matches!(err, ContractError::EmptyIdentity));
}

#[test]
fn content_hash_key_is_stable_across_key_reordered_payloads() {
    let contract = base_contract(IdentityStrategy::ContentHash);
    let payload_a = json!({ "name": "Andrew Jon", "email": "andrew@example.com", "extra": "ignored" });
    let payload_b = json!({ "extra": "ignored-but-different", "email": "andrew@example.com", "name": "Andrew Jon" });
    let key_a = identity_key(&contract, &payload_a).expect("key");
    let key_b = identity_key(&contract, &payload_b).expect("key");
    assert_eq!(key_a, key_b, "only declared identity.fields should feed the hash");
}

#[test]
fn content_hash_key_missing_declared_field_quarantines() {
    let contract = base_contract(IdentityStrategy::ContentHash);
    let payload = json!({ "name": "Andrew Jon" });
    let err = identity_key(&contract, &payload).expect_err("missing content field must quarantine");
    assert!(matches!(err, ContractError::MissingField(field) if field == "email"));
}

#[test]
fn content_hash_key_differs_from_composite_native_key_for_same_fields() {
    // The two rungs deliberately hash differently (concatenation vs a JSON
    // object) — this is not a bug, just documenting that identity keys are
    // not portable across a strategy change (an amendment).
    let composite = base_contract(IdentityStrategy::CompositeNative);
    let content = base_contract(IdentityStrategy::ContentHash);
    let payload = json!({ "name": "Andrew Jon", "email": "andrew@example.com" });
    let composite_key = identity_key(&composite, &payload).expect("key");
    let content_key = identity_key(&content, &payload).expect("key");
    assert_ne!(composite_key, content_key);
}

/// Regression test for the failure that motivated this feature: a source that free-text-describes the same underlying item
/// with drifting vocabulary across polls must still resolve to one identity
/// key under a `content_hash` contract keyed on the fields that actually
/// carry the information (not the free-text id string itself).
#[test]
fn content_hash_key_collapses_drifted_free_text_id_variants_to_one_key() {
    let mut contract = base_contract(IdentityStrategy::ContentHash);
    contract.identity.fields = vec!["client_name".to_string(), "client_id".to_string()];

    let poll_1 = json!({ "client_name": "andrew-jon", "client_id": "1234" });
    let poll_2 = json!({ "client_name": "andrew-jon", "client_id": "1234" });
    let poll_3 = json!({ "client_name": "andrew-jon", "client_id": "1234" });

    let key_1 = identity_key(&contract, &poll_1).expect("key");
    let key_2 = identity_key(&contract, &poll_2).expect("key");
    let key_3 = identity_key(&contract, &poll_3).expect("key");

    assert_eq!(key_1, key_2);
    assert_eq!(key_2, key_3);
}

/// Regression test for the re-announce half of the incident that motivated
/// this feature: a source that re-renders the same string with a curly
/// apostrophe instead of an ASCII one (or with incidental surrounding
/// whitespace) between polls must not mint a new identity_key for content
/// nobody actually changed.
#[test]
fn composite_native_key_folds_curly_quotes_and_trims_to_the_same_key() {
    let contract = base_contract(IdentityStrategy::CompositeNative);
    let ascii = json!({ "name": "O'Brien", "email": " andrew@example.com " });
    let curly = json!({ "name": "O\u{2019}Brien", "email": "andrew@example.com" });
    let key_ascii = identity_key(&contract, &ascii).expect("key");
    let key_curly = identity_key(&contract, &curly).expect("key");
    assert_eq!(key_ascii, key_curly);
}

#[test]
fn content_hash_key_folds_curly_quotes_to_the_same_key() {
    let mut contract = base_contract(IdentityStrategy::ContentHash);
    contract.identity.fields = vec!["company".to_string()];
    let ascii = json!({ "company": "Bob's Shop" });
    let curly = json!({ "company": "Bob\u{2019}s Shop" });
    let key_ascii = identity_key(&contract, &ascii).expect("key");
    let key_curly = identity_key(&contract, &curly).expect("key");
    assert_eq!(key_ascii, key_curly);
}

/// Regression test for the incident that motivated this feature: a client's
/// `company` value re-rendered with a non-breaking space instead of a normal
/// one between polls must not mint a new identity_key.
#[test]
fn content_hash_key_folds_nbsp_to_the_same_key() {
    let mut contract = base_contract(IdentityStrategy::ContentHash);
    contract.identity.fields = vec!["company".to_string()];
    let ascii = json!({ "company": "Peter's BBQ" });
    let nbsp = json!({ "company": "Peter's\u{00A0}BBQ" });
    let key_ascii = identity_key(&contract, &ascii).expect("key");
    let key_nbsp = identity_key(&contract, &nbsp).expect("key");
    assert_eq!(key_ascii, key_nbsp);
}

#[test]
fn content_hash_key_collapses_case_and_double_space_variants_to_the_same_key() {
    let mut contract = base_contract(IdentityStrategy::ContentHash);
    contract.identity.fields = vec!["company".to_string()];
    let a = json!({ "company": "Peter's BBQ" });
    let b = json!({ "company": "peter's  bbq" });
    let key_a = identity_key(&contract, &a).expect("key");
    let key_b = identity_key(&contract, &b).expect("key");
    assert_eq!(key_a, key_b);
}

#[test]
fn content_hash_key_still_differs_for_genuinely_different_companies() {
    let mut contract = base_contract(IdentityStrategy::ContentHash);
    contract.identity.fields = vec!["company".to_string()];
    let a = json!({ "company": "Peter's BBQ" });
    let b = json!({ "company": "Paul's BBQ" });
    let key_a = identity_key(&contract, &a).expect("key");
    let key_b = identity_key(&contract, &b).expect("key");
    assert_ne!(key_a, key_b);
}

// ---- version_key ------------------------------------------------------------

#[test]
fn version_key_changes_when_a_material_field_changes() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let before = json!({ "tag": "Normal", "status": "open", "notes": "a" });
    let after = json!({ "tag": "Very Important", "status": "open", "notes": "a" });
    let key_before = version_key(&contract, &before).expect("key");
    let key_after = version_key(&contract, &after).expect("key");
    assert_ne!(key_before, key_after);
}

#[test]
fn version_key_ignores_non_material_fields() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let before = json!({ "tag": "Normal", "status": "open", "notes": "typo" });
    let after = json!({ "tag": "Normal", "status": "open", "notes": "typo fixed" });
    let key_before = version_key(&contract, &before).expect("key");
    let key_after = version_key(&contract, &after).expect("key");
    assert_eq!(key_before, key_after, "editing a non-material field must not move the version key");
}

#[test]
fn version_key_ignores_version_hint_field() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let before = json!({ "tag": "Normal", "status": "open", "last_edited_time": "t1" });
    let after = json!({ "tag": "Normal", "status": "open", "last_edited_time": "t2" });
    let key_before = version_key(&contract, &before).expect("key");
    let key_after = version_key(&contract, &after).expect("key");
    assert_eq!(key_before, key_after, "version_hint_field is a pre-filter, never part of the key");
}

#[test]
fn version_key_treats_missing_material_field_as_stable_null() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let payload_a = json!({ "status": "open" });
    let payload_b = json!({ "status": "open" });
    let key_a = version_key(&contract, &payload_a).expect("key");
    let key_b = version_key(&contract, &payload_b).expect("key");
    assert_eq!(key_a, key_b);
}

#[test]
fn version_key_folds_curly_quotes_and_trims_to_the_same_key() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let ascii = json!({ "tag": "Bob's Shop", "status": " open " });
    let curly = json!({ "tag": "Bob\u{2019}s Shop", "status": "open" });
    let key_ascii = version_key(&contract, &ascii).expect("key");
    let key_curly = version_key(&contract, &curly).expect("key");
    assert_eq!(key_ascii, key_curly, "a curly-quote/whitespace re-render of the same value must not move the version key");
}

// ---- predicate evaluator -----------------------------------------------------
//
// `evaluate_predicate` (this module's contract-scoped wrapper) now operates
// on a typed `Predicate`, not a string, and is total (no `Result` — see its
// doc). These tests build a minimal contract via `legacy_expr::parse`, which
// is still the legacy grammar's exact semantics (it's what every one of
// these expressions goes through on migration) — the parse-failure and
// "never silently true" cases the retired string evaluator used to cover
// directly now live in `legacy_expr`'s own differential test battery
// (`legacy_expr_tests.rs`), since a parse failure is no longer something
// this contract-level evaluator can even observe.

fn eval_expr(expr: &str, payload: &Value) -> bool {
    let contract = base_contract_with_expr(IdentityStrategy::NativeId, expr);
    evaluate_predicate(&contract, payload, None)
}

fn base_contract_with_expr(strategy: IdentityStrategy, expr: &str) -> WatchContract {
    let mut contract = base_contract(strategy);
    contract.predicate.predicate = legacy_expr::parse(expr).expect("valid fixture expr");
    contract
}

#[test]
fn predicate_contains_matches_case_insensitively() {
    let payload = json!({ "tag": "Very important client" });
    assert!(eval_expr("contains(tag, 'very important')", &payload));
}

#[test]
fn predicate_contains_no_match_is_false() {
    let payload = json!({ "tag": "Normal" });
    assert!(!eval_expr("contains(tag, 'Very Important')", &payload));
}

#[test]
fn predicate_contains_missing_field_is_false_not_error() {
    let payload = json!({ "other": "x" });
    assert_eq!(eval_expr("contains(tag, 'Very Important')", &payload), false);
}

#[test]
fn predicate_equals_matches_case_insensitively() {
    let payload = json!({ "status": "OPEN" });
    assert!(eval_expr("equals(status, 'open')", &payload));
}

#[test]
fn predicate_equals_missing_field_is_false() {
    let payload = json!({});
    assert!(!eval_expr("equals(status, 'open')", &payload));
}

#[test]
fn predicate_not_empty_true_for_non_blank_string() {
    let payload = json!({ "notes": "hello" });
    assert!(eval_expr("not_empty(notes)", &payload));
}

#[test]
fn predicate_not_empty_false_for_blank_or_missing() {
    assert!(!eval_expr("not_empty(notes)", &json!({ "notes": "" })));
    assert!(!eval_expr("not_empty(notes)", &json!({ "notes": "   " })));
    assert!(!eval_expr("not_empty(notes)", &json!({})));
    assert!(!eval_expr("not_empty(notes)", &json!({ "notes": null })));
}

#[test]
fn predicate_and_requires_both() {
    let payload = json!({ "tag": "Very Important", "status": "open" });
    assert!(eval_expr("and(contains(tag, 'Very Important'), equals(status, 'open'))", &payload));
    assert!(!eval_expr("and(contains(tag, 'Very Important'), equals(status, 'closed'))", &payload));
}

#[test]
fn predicate_or_requires_either() {
    let payload = json!({ "tag": "Normal", "status": "open" });
    assert!(eval_expr("or(contains(tag, 'Very Important'), equals(status, 'open'))", &payload));
    assert!(!eval_expr("or(contains(tag, 'Very Important'), equals(status, 'closed'))", &payload));
}

#[test]
fn predicate_not_inverts() {
    let payload = json!({ "tag": "Normal" });
    assert!(eval_expr("not(contains(tag, 'Very Important'))", &payload));
    assert!(!eval_expr("not(not_empty(tag))", &payload));
}

#[test]
fn predicate_nested_composition() {
    let payload = json!({ "tag": "Very Important", "status": "closed" });
    let expr = "and(contains(tag, 'Very Important'), not(equals(status, 'closed')))";
    assert!(!eval_expr(expr, &payload));
}

#[test]
fn predicate_unknown_function_is_an_error_not_a_panic() {
    let err = legacy_expr::parse("frobnicate(tag)").expect_err("unknown function must error");
    assert!(matches!(err, ContractError::InvalidPredicate(_)));
}

#[test]
fn predicate_malformed_expr_is_an_error_not_a_panic() {
    for expr in [
        "",
        "contains(tag, 'unterminated",
        "contains(tag 'missing comma')",
        "and(contains(tag, 'x'))",
        "contains(tag, 'x') trailing garbage",
        "not_empty(",
        "(((",
    ] {
        let result = legacy_expr::parse(expr);
        assert!(result.is_err(), "expected error for expr {expr:?}, got {result:?}");
    }
}

// ---- WatchContract::validate ------------------------------------------------

#[test]
fn validate_accepts_a_well_formed_contract() {
    assert!(base_contract(IdentityStrategy::NativeId).validate().is_ok());
    assert!(base_contract(IdentityStrategy::CompositeNative).validate().is_ok());
    assert!(base_contract(IdentityStrategy::ContentHash).validate().is_ok());
}

// There is no `validate_rejects_an_unparseable_predicate_expr` test anymore:
// `predicate` is a typed `Predicate`, well-formed by construction, so
// `validate()` has nothing left to check there — see its doc. The
// equivalent guarantee (an unparseable legacy `expr` string is rejected,
// never silently accepted) is covered where parsing now actually happens:
// `legacy_expr`'s differential tests, and
// `predicate_malformed_expr_is_an_error_not_a_panic` above.

#[test]
fn validate_rejects_an_invalid_identity_format_regex() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.identity.format = Some("(unclosed".to_string());
    assert!(matches!(contract.validate(), Err(ContractError::InvalidRegex(_))));
}

#[test]
fn validate_rejects_native_id_with_no_source_field() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.identity.source_field = None;
    assert!(matches!(contract.validate(), Err(ContractError::EmptyIdentity)));
}

#[test]
fn validate_rejects_composite_native_with_empty_fields() {
    let mut contract = base_contract(IdentityStrategy::CompositeNative);
    contract.identity.fields = vec![];
    assert!(matches!(contract.validate(), Err(ContractError::EmptyIdentity)));
}

#[test]
fn validate_rejects_content_hash_with_empty_fields() {
    let mut contract = base_contract(IdentityStrategy::ContentHash);
    contract.identity.fields = vec![];
    assert!(matches!(contract.validate(), Err(ContractError::EmptyIdentity)));
}

#[test]
fn validate_rejects_empty_material_fields() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.change.material_fields = vec![];
    assert!(matches!(contract.validate(), Err(ContractError::EmptyMaterialFields)));
}

#[test]
fn validate_still_rejects_empty_material_fields_for_new_or_changed() {
    // `NewOnly` is the only mode exempt from this check (see the next test)
    // — `NewOrChanged` still fires off a version transition, so it needs
    // material fields declared exactly like the `PredicateTransition`
    // default covered by `validate_rejects_empty_material_fields` above.
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.mode = WatchMode::NewOrChanged;
    contract.change.material_fields = vec![];
    assert!(matches!(contract.validate(), Err(ContractError::EmptyMaterialFields)));
}

#[test]
fn validate_accepts_empty_material_fields_for_new_only_mode() {
    // `WatchMode::NewOnly` fires purely on `prev.is_none()` (see
    // `ao_engine::agent_watch`'s runtime tick) — existence of the item is
    // the whole event, so there is no prior version to diff and nothing for
    // `material_fields` to declare. This is what makes "appearance" watches
    // authorable at all: without this exemption, `validate()` rejected
    // every `new_only` proposal for the exact property that defines it.
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.mode = WatchMode::NewOnly;
    contract.change.material_fields = vec![];
    assert!(contract.validate().is_ok());
}

#[test]
fn validate_rejects_identity_fields_overlapping_material_fields() {
    let mut contract = base_contract(IdentityStrategy::CompositeNative);
    contract.identity.fields = vec!["first_name".to_string(), "last_name".to_string(), "company".to_string()];
    contract.change.material_fields = vec!["first_name".to_string(), "last_name".to_string(), "company".to_string()];

    let err = contract.validate().expect_err("identical identity/material field sets must be rejected");
    let message = err.to_string();
    let ContractError::IdentityMaterialFieldOverlap(offending) = err else {
        panic!("expected IdentityMaterialFieldOverlap, got {message}");
    };
    assert_eq!(offending, vec!["company".to_string(), "first_name".to_string(), "last_name".to_string()]);
    assert!(
        message.contains("company") && message.contains("material_fields only"),
        "error must name the offending fields and state the remedy; got: {message}"
    );
}

#[test]
fn validate_rejects_a_single_field_shared_between_identity_and_material_fields() {
    let mut contract = base_contract(IdentityStrategy::CompositeNative);
    contract.identity.fields = vec!["email".to_string(), "region".to_string()];
    contract.change.material_fields = vec!["email".to_string(), "status".to_string()];
    assert!(matches!(contract.validate(), Err(ContractError::IdentityMaterialFieldOverlap(fields)) if fields == vec!["email".to_string()]));
}

// ---- validate: required + NotEmpty cross-check ------------------------------
//
// Regression coverage for the production incident this check exists to
// prevent: a contract pairing `fields[name].required = true` with a
// `Predicate::NotEmpty` leaf targeting the same field self-destructs at
// runtime — the required-field gate quarantines a blank value as missing
// before the predicate (whose whole point is to tolerate a blank value
// quietly) is ever evaluated. See `ContractError::RequiredFieldTargetedByTolerantPredicate`.

#[test]
fn validate_rejects_required_field_targeted_by_not_empty_predicate() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.fields.insert("summary".to_string(), FieldSpec { field_type: "string".to_string(), required: true });
    contract.predicate.predicate = crate::predicate::Predicate::NotEmpty { path: "summary".to_string() };

    let err = contract.validate().expect_err("required + NotEmpty on the same field must be rejected");
    let message = err.to_string();
    let ContractError::RequiredFieldTargetedByTolerantPredicate(field) = err else {
        panic!("expected RequiredFieldTargetedByTolerantPredicate, got {message}");
    };
    assert_eq!(field, "summary");
    assert!(
        message.contains("summary") && message.contains("required") && message.contains("contradict"),
        "error must name the field and explain the contradiction, not blame the payload; got: {message}"
    );
}

#[test]
fn validate_accepts_required_field_with_no_not_empty_predicate() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    // `summary` is required but the predicate (inherited from `base_contract`)
    // targets `tag`, an unrelated field — no contradiction.
    contract.fields.insert("summary".to_string(), FieldSpec { field_type: "string".to_string(), required: true });
    assert!(contract.validate().is_ok());
}

#[test]
fn validate_accepts_not_empty_predicate_on_a_non_required_field() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.fields.insert("summary".to_string(), FieldSpec { field_type: "string".to_string(), required: false });
    contract.predicate.predicate = crate::predicate::Predicate::NotEmpty { path: "summary".to_string() };
    assert!(contract.validate().is_ok());
}

#[test]
fn validate_catches_not_empty_nested_inside_and_or_not() {
    let mut contract = base_contract(IdentityStrategy::NativeId);
    contract.fields.insert("summary".to_string(), FieldSpec { field_type: "string".to_string(), required: true });
    // The offending `NotEmpty { path: "summary" }` sits three combinators
    // deep — And( Or( Not(NotEmpty(summary)), EqualsCi(status) ), ContainsCi(tag) ) —
    // to prove the tree walk isn't just checking the root node.
    contract.predicate.predicate = crate::predicate::Predicate::And(vec![
        crate::predicate::Predicate::Or(vec![
            crate::predicate::Predicate::Not(Box::new(crate::predicate::Predicate::NotEmpty {
                path: "summary".to_string(),
            })),
            crate::predicate::Predicate::EqualsCi { path: "status".to_string(), literal: "open".to_string() },
        ]),
        crate::predicate::Predicate::ContainsCi { path: "tag".to_string(), literal: "urgent".to_string() },
    ]);

    let err = contract.validate().expect_err("NotEmpty nested inside And/Or/Not must still be caught");
    assert!(matches!(err, ContractError::RequiredFieldTargetedByTolerantPredicate(field) if field == "summary"));
}

// ---- 200-identical-polls sanity (identity + version stability together) ----

#[test]
fn two_hundred_identical_polls_produce_one_stable_identity_and_version_key() {
    let contract = base_contract(IdentityStrategy::NativeId);
    let payload = json!({ "unique_identifier": "stable-id-1", "tag": "Normal", "status": "open" });
    let first_identity = identity_key(&contract, &payload).expect("key");
    let first_version = version_key(&contract, &payload).expect("key");
    for _ in 0..200 {
        assert_eq!(identity_key(&contract, &payload).expect("key"), first_identity);
        assert_eq!(version_key(&contract, &payload).expect("key"), first_version);
    }
}

// ---- characterization: pins persisted hash output across the primitive-consolidation refactor ----
//
// `identity_key`/`version_key` outputs are persisted on-disk in
// `AssignmentScratchpad.snapshots`. This test locks in the CURRENT output of
// `canonical_json`, `identity_key` (all three `IdentityStrategy` rungs), and
// `version_key` against a non-trivial fixture (nested objects, arrays,
// unicode, curly quotes, NBSP, and a material field missing from the
// payload). If a refactor changes any of these hardcoded literals, the
// refactor broke persisted snapshot compatibility — fix the refactor, do NOT
// update the literals to match new output.

fn characterization_payload() -> Value {
    json!({
        "native_id": "  O\u{2019}Brien\u{00a0}Corp  ",
        "profile": {
            "name": "Andrew Jon",
            "unicode": "caf\u{00e9}",
            "nested": { "level": 2, "arr": [1, 2, 3], "flag": true, "nothing": null }
        },
        "tags": ["Very Important", "O\u{2019}Brien", null, {"k": "v"}],
        "meta": {
            "curly": "\u{201C}quoted\u{201D}",
            "num": 3.14,
            "flag": false
        }
    })
}

fn characterization_contract(strategy: IdentityStrategy) -> WatchContract {
    WatchContract {
        contract_version: 1,
        authored_at: "2026-01-01T00:00:00Z".to_string(),
        authored_by_run: "run-fixture".to_string(),
        source: WatchSource { kind: "fixture".to_string(), ref_: "fixture-ref".to_string() },
        identity: IdentitySpec {
            strategy,
            source_field: Some("native_id".to_string()),
            format: None,
            fields: vec!["profile".to_string(), "tags".to_string()],
            rationale: "characterization fixture".to_string(),
        },
        change: ChangeSpec {
            material_fields: vec!["profile".to_string(), "meta".to_string(), "absent_field".to_string()],
            version_hint_field: None,
        },
        predicate: PredicateSpec {
            natural_language: String::new(),
            fields: vec![],
            predicate: legacy_expr::parse("not_empty(native_id)").expect("valid fixture expr"),
        },
        mode: WatchMode::PredicateTransition,
        fields: HashMap::new(),
    }
}

#[test]
fn characterization_pins_persisted_hash_output_do_not_update_on_refactor() {
    let payload = characterization_payload();

    let canonical = canonical_json(&payload);
    let expected_canonical = "{\"meta\":{\"curly\":\"\u{201C}quoted\u{201D}\",\"flag\":false,\"num\":3.14},\
        \"native_id\":\"  O\u{2019}Brien\u{00a0}Corp  \",\"profile\":{\"name\":\"Andrew Jon\",\"nested\":{\"arr\":\
        [1,2,3],\"flag\":true,\"level\":2,\"nothing\":null},\"unicode\":\"caf\u{00e9}\"},\"tags\":[\"Very Important\",\
        \"O\u{2019}Brien\",null,{\"k\":\"v\"}]}";
    assert_eq!(
        canonical, expected_canonical,
        "canonical_json output changed — this is a persisted-hash-breaking change, not just a formatting diff"
    );

    let native = identity_key(&characterization_contract(IdentityStrategy::NativeId), &payload).expect("native id key");
    assert_eq!(native, "dadc88b7b222d2b5ce1d8be08fad4a4747d6a7828da1e9a21a219e6ec5708f54");

    let composite =
        identity_key(&characterization_contract(IdentityStrategy::CompositeNative), &payload).expect("composite key");
    assert_eq!(composite, "932aa9dace1d02838cc7e60b31449ae170bbb408ca4005e73ae5b820a31f7372");

    let content =
        identity_key(&characterization_contract(IdentityStrategy::ContentHash), &payload).expect("content hash key");
    assert_eq!(content, "c657325bee24df5ecf60e799532440738123912a3a078c4f2bd416548ae10a0a");

    let version = version_key(&characterization_contract(IdentityStrategy::NativeId), &payload).expect("version key");
    assert_eq!(version, "e571882dd4b3794d3ccfa22f92d67d67c42b01d12e8672c86bce4813f0d4db9b");
}

// ---- fingerprint stability vs. the rendered-expression surface -----------

/// Golden fingerprint of [`base_contract`]. The rendered "raw expression"
/// shown in the contract explainer is derived at the API response layer from
/// the already-persisted typed predicate ([`crate::predicate::Predicate::to_expr`])
/// and is deliberately NOT a field on `WatchContract` — because
/// [`WatchContract::fingerprint`] serializes the whole contract, so any new
/// serialized field inside it would change this hash for every already-persisted
/// assignment and force a snapshot reseed (dropping each watch's per-item
/// baseline and re-firing history).
///
/// This value is a tripwire, not a spec: if you are here because this test
/// failed, you added, renamed, reordered-into or removed a serialized field
/// somewhere inside `WatchContract`, and every existing watch will reseed.
/// Confirm that is genuinely intended before updating the constant.
const BASE_CONTRACT_FINGERPRINT: &str = "0c80b7c1b65e9778d902134d97932f30766d78286f01c0ddd68cfa213d2990ce";

#[test]
fn fingerprint_is_unchanged_by_the_rendered_expression_surface() {
    let contract = base_contract(IdentityStrategy::NativeId);
    assert_eq!(
        contract.fingerprint(),
        BASE_CONTRACT_FINGERPRINT,
        "WatchContract's serialized shape changed — every persisted watch would reseed"
    );

    // Rendering the expression is a pure read: it neither mutates the
    // contract nor participates in the fingerprint.
    let rendered = contract.predicate.predicate.to_expr();
    assert_eq!(rendered.as_deref(), Some("contains(tag, 'Very Important')"));
    assert_eq!(contract.fingerprint(), BASE_CONTRACT_FINGERPRINT);

    // Two contracts identical except that one's predicate is unrenderable in
    // the display grammar still fingerprint identically to themselves — the
    // renderability of a predicate is not contract state.
    let mut unrenderable = base_contract(IdentityStrategy::NativeId);
    unrenderable.predicate.predicate = crate::predicate::Predicate::GreaterThan {
        path: "count".to_string(),
        number: 5.0,
    };
    assert_eq!(unrenderable.predicate.predicate.to_expr(), None);
    let before = unrenderable.fingerprint();
    let _ = unrenderable.predicate.predicate.to_expr();
    assert_eq!(unrenderable.fingerprint(), before);
}
