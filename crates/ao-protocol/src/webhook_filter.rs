//! Declarative, pre-agent relevance filters for `AssignmentTrigger::Webhook`
//! routes.
//!
//! A filter is evaluated against the raw inbound JSON payload, before any
//! agent run starts, so an irrelevant delivery costs zero tokens. The tree
//! is declarative on purpose — no embedded scripts — so it can be rendered
//! and edited by a form UI and matched with a pure, synchronous function
//! that is trivial to unit test and to preview against a sample payload.

use std::fs;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single field-level test: read `field` (a dot-path into the payload,
/// e.g. `"pull_request.title"` or an array index like `"items.0.id"`) and
/// evaluate `op` against whatever value (if any) is found there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookFieldFilter {
    /// Dot-path into the payload. Numeric segments index into JSON arrays.
    pub field: String,
    #[serde(flatten)]
    pub op: WebhookFilterOp,
}

/// The comparison applied to the value found at [`WebhookFieldFilter::field`].
///
/// `Exists`/`Missing` test presence alone; every other variant treats a
/// missing field as "does not match" (see [`WebhookFieldFilter::matches`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WebhookFilterOp {
    /// Found value equals `value` (JSON equality — string/number/bool/etc).
    Equals { value: Value },
    /// Found value does not equal `value`. Also true when the field is
    /// missing, since "missing" is never equal to a present value.
    NotEquals { value: Value },
    /// Found value is a string containing `value` as a substring, or an
    /// array containing `value` as an element.
    Contains { value: Value },
    /// Found value equals one of `values`.
    In { values: Vec<Value> },
    /// Found value (coerced to a string) equals one line (trimmed) of the
    /// file at `path`. Reads the file synchronously on every call — the
    /// route-authoring UI is expected to point this at small allowlist
    /// files (e.g. a list of trusted usernames), not anything large or
    /// hot-reloaded per request. A missing or unreadable file never matches
    /// rather than erroring, so a route with a bad path fails closed.
    InFile { path: String },
    /// Found value (coerced to a string) matches `pattern` as a regex.
    /// An invalid `pattern` never matches rather than erroring.
    Regex { pattern: String },
    /// The field is present (any value, including `null` is NOT present —
    /// `serde_json::Value::Null` means the key existed and was `null`; both
    /// count as "exists" here, matching JSON semantics of key presence).
    Exists,
    /// The field is absent.
    Missing,
}

/// A declarative filter tree: a single field test, or `all`/`any`/`not`
/// combinators over subtrees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WebhookFilter {
    /// Every subtree must match. Vacuously `true` for an empty list.
    All { all: Vec<WebhookFilter> },
    /// At least one subtree must match. Vacuously `false` for an empty list.
    Any { any: Vec<WebhookFilter> },
    /// The subtree must not match.
    Not { not: Box<WebhookFilter> },
    /// A single field-level test.
    Field(WebhookFieldFilter),
}

impl WebhookFilter {
    /// Evaluate this filter tree against `payload`. Pure and synchronous —
    /// no network or async I/O — except for [`WebhookFilterOp::InFile`],
    /// which reads its configured file from local disk on each call.
    pub fn matches(&self, payload: &Value) -> bool {
        match self {
            WebhookFilter::All { all } => all.iter().all(|f| f.matches(payload)),
            WebhookFilter::Any { any } => any.iter().any(|f| f.matches(payload)),
            WebhookFilter::Not { not } => !not.matches(payload),
            WebhookFilter::Field(field_filter) => field_filter.matches(payload),
        }
    }
}

impl WebhookFieldFilter {
    fn matches(&self, payload: &Value) -> bool {
        let found = get_dot_path(payload, &self.field);
        match &self.op {
            WebhookFilterOp::Exists => found.is_some(),
            WebhookFilterOp::Missing => found.is_none(),
            WebhookFilterOp::Equals { value } => found == Some(value),
            WebhookFilterOp::NotEquals { value } => found != Some(value),
            WebhookFilterOp::Contains { value } => match found {
                Some(Value::String(s)) => match value {
                    Value::String(needle) => s.contains(needle.as_str()),
                    _ => false,
                },
                Some(Value::Array(items)) => items.contains(value),
                _ => false,
            },
            WebhookFilterOp::In { values } => found.map(|v| values.contains(v)).unwrap_or(false),
            WebhookFilterOp::InFile { path } => {
                let Some(found) = found else { return false };
                let Some(needle) = value_as_str(found) else { return false };
                match fs::read_to_string(path) {
                    Ok(contents) => contents.lines().map(str::trim).any(|line| line == needle),
                    Err(_) => false,
                }
            }
            WebhookFilterOp::Regex { pattern } => {
                let Some(found) = found else { return false };
                let Some(haystack) = value_as_str(found) else { return false };
                match Regex::new(pattern) {
                    Ok(re) => re.is_match(&haystack),
                    Err(_) => false,
                }
            }
        }
    }
}

/// Render a JSON value as the string a regex/in_file comparison should run
/// against: strings pass through as-is (no surrounding quotes), everything
/// else falls back to its JSON serialization.
fn value_as_str(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// True when `event_type` (e.g. the value of a GitHub `X-GitHub-Event`
/// header) is allowed through by a route's `events` allowlist. An empty
/// allowlist allows every event type through to `filters`. A non-empty
/// allowlist requires a *present, matching* event type — a request with no
/// event-type header at all fails closed rather than being treated as
/// "matches everything", since that would silently defeat the allowlist for
/// any sender that doesn't set the header.
pub fn event_type_allowed(events: &[String], event_type: Option<&str>) -> bool {
    events.is_empty() || event_type.map(|et| events.iter().any(|e| e == et)).unwrap_or(false)
}

/// Resolve a dot-path into `value`. Numeric segments index into arrays;
/// anything else looks up an object key. Returns `None` if any segment
/// fails to resolve (missing key, out-of-range index, or indexing into a
/// scalar) or the path is empty.
pub(crate) fn get_dot_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    let mut resolved_any = false;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        resolved_any = true;
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(arr) => arr.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    if resolved_any {
        Some(current)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(field: &str, op: WebhookFilterOp) -> WebhookFilter {
        WebhookFilter::Field(WebhookFieldFilter { field: field.to_string(), op })
    }

    fn sample_payload() -> Value {
        serde_json::json!({
            "action": "opened",
            "pull_request": {
                "title": "Fix the flaky retry loop",
                "number": 42,
                "labels": ["bug", "needs-review"],
            },
            "sender": { "login": "octocat" },
        })
    }

    #[test]
    fn equals_matches_present_value() {
        let payload = sample_payload();
        let f = field("action", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) });
        assert!(f.matches(&payload));

        let f = field("action", WebhookFilterOp::Equals { value: Value::String("closed".to_string()) });
        assert!(!f.matches(&payload));
    }

    #[test]
    fn equals_does_not_match_missing_field() {
        let payload = sample_payload();
        let f = field("nope", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) });
        assert!(!f.matches(&payload));
    }

    #[test]
    fn not_equals_matches_missing_and_different_values() {
        let payload = sample_payload();
        let f = field("action", WebhookFilterOp::NotEquals { value: Value::String("closed".to_string()) });
        assert!(f.matches(&payload));

        let f = field("action", WebhookFilterOp::NotEquals { value: Value::String("opened".to_string()) });
        assert!(!f.matches(&payload));

        let f = field("nope", WebhookFilterOp::NotEquals { value: Value::String("opened".to_string()) });
        assert!(f.matches(&payload), "missing field is never equal to a present value");
    }

    #[test]
    fn contains_matches_substring_and_array_element() {
        let payload = sample_payload();
        let f = field(
            "pull_request.title",
            WebhookFilterOp::Contains { value: Value::String("flaky".to_string()) },
        );
        assert!(f.matches(&payload));

        let f = field(
            "pull_request.title",
            WebhookFilterOp::Contains { value: Value::String("nope".to_string()) },
        );
        assert!(!f.matches(&payload));

        let f = field(
            "pull_request.labels",
            WebhookFilterOp::Contains { value: Value::String("bug".to_string()) },
        );
        assert!(f.matches(&payload));

        let f = field(
            "pull_request.labels",
            WebhookFilterOp::Contains { value: Value::String("wontfix".to_string()) },
        );
        assert!(!f.matches(&payload));
    }

    #[test]
    fn contains_on_non_string_non_array_never_matches() {
        let payload = sample_payload();
        let f = field(
            "pull_request.number",
            WebhookFilterOp::Contains { value: Value::from(42) },
        );
        assert!(!f.matches(&payload));
    }

    #[test]
    fn in_matches_membership() {
        let payload = sample_payload();
        let f = field(
            "action",
            WebhookFilterOp::In {
                values: vec![Value::String("opened".to_string()), Value::String("reopened".to_string())],
            },
        );
        assert!(f.matches(&payload));

        let f = field(
            "action",
            WebhookFilterOp::In { values: vec![Value::String("closed".to_string())] },
        );
        assert!(!f.matches(&payload));

        let f = field(
            "nope",
            WebhookFilterOp::In { values: vec![Value::String("closed".to_string())] },
        );
        assert!(!f.matches(&payload), "missing field never matches an in() list");
    }

    #[test]
    fn in_file_matches_a_trimmed_line() {
        let payload = sample_payload();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allowed_senders.txt");
        std::fs::write(&path, "octocat\nother-user\n").unwrap();

        let f = field("sender.login", WebhookFilterOp::InFile { path: path.to_string_lossy().to_string() });
        assert!(f.matches(&payload));

        let f = field(
            "sender.login",
            WebhookFilterOp::InFile { path: dir.path().join("missing.txt").to_string_lossy().to_string() },
        );
        assert!(!f.matches(&payload), "unreadable file fails closed rather than erroring");
    }

    #[test]
    fn in_file_does_not_match_when_line_absent() {
        let payload = sample_payload();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allowed_senders.txt");
        std::fs::write(&path, "someone-else\n").unwrap();

        let f = field("sender.login", WebhookFilterOp::InFile { path: path.to_string_lossy().to_string() });
        assert!(!f.matches(&payload));
    }

    #[test]
    fn regex_matches_pattern() {
        let payload = sample_payload();
        let f = field(
            "pull_request.title",
            WebhookFilterOp::Regex { pattern: r"(?i)flaky".to_string() },
        );
        assert!(f.matches(&payload));

        let f = field(
            "pull_request.title",
            WebhookFilterOp::Regex { pattern: r"^Add ".to_string() },
        );
        assert!(!f.matches(&payload));
    }

    #[test]
    fn regex_invalid_pattern_never_matches() {
        let payload = sample_payload();
        let f = field("pull_request.title", WebhookFilterOp::Regex { pattern: "(".to_string() });
        assert!(!f.matches(&payload));
    }

    #[test]
    fn exists_and_missing() {
        let payload = sample_payload();
        assert!(field("action", WebhookFilterOp::Exists).matches(&payload));
        assert!(!field("nope", WebhookFilterOp::Exists).matches(&payload));
        assert!(!field("action", WebhookFilterOp::Missing).matches(&payload));
        assert!(field("nope", WebhookFilterOp::Missing).matches(&payload));
    }

    #[test]
    fn all_requires_every_subtree_and_is_vacuously_true_when_empty() {
        let payload = sample_payload();
        let f = WebhookFilter::All {
            all: vec![
                field("action", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) }),
                field("sender.login", WebhookFilterOp::Equals { value: Value::String("octocat".to_string()) }),
            ],
        };
        assert!(f.matches(&payload));

        let f = WebhookFilter::All {
            all: vec![
                field("action", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) }),
                field("sender.login", WebhookFilterOp::Equals { value: Value::String("nope".to_string()) }),
            ],
        };
        assert!(!f.matches(&payload));

        assert!(WebhookFilter::All { all: vec![] }.matches(&payload));
    }

    #[test]
    fn any_requires_one_subtree_and_is_vacuously_false_when_empty() {
        let payload = sample_payload();
        let f = WebhookFilter::Any {
            any: vec![
                field("action", WebhookFilterOp::Equals { value: Value::String("closed".to_string()) }),
                field("sender.login", WebhookFilterOp::Equals { value: Value::String("octocat".to_string()) }),
            ],
        };
        assert!(f.matches(&payload));

        let f = WebhookFilter::Any {
            any: vec![
                field("action", WebhookFilterOp::Equals { value: Value::String("closed".to_string()) }),
                field("sender.login", WebhookFilterOp::Equals { value: Value::String("nope".to_string()) }),
            ],
        };
        assert!(!f.matches(&payload));

        assert!(!WebhookFilter::Any { any: vec![] }.matches(&payload));
    }

    #[test]
    fn not_inverts_the_subtree() {
        let payload = sample_payload();
        let f = WebhookFilter::Not {
            not: Box::new(field("action", WebhookFilterOp::Equals { value: Value::String("closed".to_string()) })),
        };
        assert!(f.matches(&payload));

        let f = WebhookFilter::Not {
            not: Box::new(field("action", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) })),
        };
        assert!(!f.matches(&payload));
    }

    #[test]
    fn nested_all_any_not_combine() {
        let payload = sample_payload();
        // all(action == opened, any(sender.login == octocat, sender.login == other), not(pull_request.number == 0))
        let f = WebhookFilter::All {
            all: vec![
                field("action", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) }),
                WebhookFilter::Any {
                    any: vec![
                        field("sender.login", WebhookFilterOp::Equals { value: Value::String("octocat".to_string()) }),
                        field("sender.login", WebhookFilterOp::Equals { value: Value::String("other".to_string()) }),
                    ],
                },
                WebhookFilter::Not {
                    not: Box::new(field("pull_request.number", WebhookFilterOp::Equals { value: Value::from(0) })),
                },
            ],
        };
        assert!(f.matches(&payload));
    }

    #[test]
    fn dot_path_indexes_into_arrays() {
        let payload = sample_payload();
        let f = field(
            "pull_request.labels.0",
            WebhookFilterOp::Equals { value: Value::String("bug".to_string()) },
        );
        assert!(f.matches(&payload));

        let f = field(
            "pull_request.labels.5",
            WebhookFilterOp::Exists,
        );
        assert!(!f.matches(&payload));
    }

    #[test]
    fn filter_json_round_trips_with_flat_field_op_shape() {
        let f = field("action", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) });
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["field"], Value::String("action".to_string()));
        assert_eq!(json["op"], Value::String("equals".to_string()));
        assert_eq!(json["value"], Value::String("opened".to_string()));

        let back: WebhookFilter = serde_json::from_value(json).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn event_type_allowed_empty_allowlist_matches_anything() {
        assert!(event_type_allowed(&[], None));
        assert!(event_type_allowed(&[], Some("pull_request")));
    }

    #[test]
    fn event_type_allowed_requires_present_matching_type() {
        let events = vec!["pull_request".to_string(), "issues".to_string()];
        assert!(event_type_allowed(&events, Some("pull_request")));
        assert!(!event_type_allowed(&events, Some("push")));
        assert!(!event_type_allowed(&events, None), "missing event-type header fails closed against a non-empty allowlist");
    }

    #[test]
    fn filter_group_json_round_trips() {
        let f = WebhookFilter::All {
            all: vec![
                field("action", WebhookFilterOp::Equals { value: Value::String("opened".to_string()) }),
                WebhookFilter::Not {
                    not: Box::new(field("sender.login", WebhookFilterOp::Missing)),
                },
            ],
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: WebhookFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
    }
}
