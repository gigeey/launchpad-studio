//! Tier 1 curated watchable-event registry (poll side).
//!
//! On the poll side, this is a hand-curated, per-connector catalog of named events ("New
//! email", "New starred email", …) that stand in for the raw MCP
//! `tool_name`/`arguments`/`cursor_path` triple a user would otherwise have
//! to compose by hand. It is a curated *front-end* over the existing poll
//! mechanism, not a parallel one: every [`CuratedEvent`] compiles down to the
//! exact same [`ConnectorPollSpec`]/[`AssignmentTrigger::ConnectorEvent`]
//! shape `schedule_runner::tick_connector_events` already polls — nothing
//! about the deterministic poller changes.
//!
//! A curated event owns:
//! - the raw MCP `tool_name` to call each poll,
//! - `default_arguments` for that call,
//! - a `cursor_path` that is **always non-blank** (curated events are
//!   hand-verified to resolve against a real result shape, so they never hit
//!   the blank-cursor no-fire gap described on
//!   [`ConnectorPollSpec::cursor_path`]),
//! - and at most a couple of `friendly_params` — named keys inside
//!   `default_arguments` a caller may override (e.g. which Gmail label to
//!   watch) without exposing the rest of the raw argument shape.
//!
//! This module is the shared surface two later rungs build on: the poll-side
//! event picker UI (fills a curated trigger from a dropdown instead of raw
//! fields) and the one-shot test-poll endpoint (calls [`compile_poll_spec`]
//! once to preview what an event would fire on). Neither is wired up yet —
//! this rung only adds the catalog and the compile-down mapping.
//!
//! **Scope of this rung:** only `gmail` ships with verified entries — its
//! `list_starred` shape mirrors the fixture already exercised in
//! `ao_protocol::assignment`'s own tests, and `list_emails` is the sibling
//! list-style tool on the same server. A `github` connector is included too,
//! operationalizing the `search_issues` example already named on
//! [`ConnectorPollSpec::tool_name`]'s own rustdoc. Additional connectors are
//! deliberately deferred rather than hand-waved with unverified tool
//! schemas — curation is meant to grow entry-by-entry as each connector's
//! real MCP tool shape is confirmed. Curation is an optimization on top of
//! the agent-driven fallback, not a gate.

use std::collections::HashMap;

use serde_json::Value;
use thiserror::Error;

use ao_protocol::assignment::{AssignmentTrigger, ConnectorPollSpec};

/// One user-facing knob inside a [`CuratedEvent`]'s `default_arguments` a
/// caller may override by key. Kept to "at most 1-2" per event per the plan
/// (Layer 1) — anything beyond that belongs behind the
/// Tier 3 "Advanced (raw)" escape hatch, not bolted onto the curated tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedFriendlyParam {
    /// Key inside the event's `default_arguments` object this param
    /// overrides when a caller supplies a value for it.
    pub arg_key: String,
    /// Short user-facing label (e.g. "Label").
    pub label: String,
    /// One-line help text describing what the param controls.
    pub description: String,
}

/// A single named, hand-curated watchable event on a connector.
#[derive(Debug, Clone, PartialEq)]
pub struct CuratedEvent {
    /// Stable id within its connector (e.g. `"new_starred_email"`). Together
    /// with the owning connector's `server_name` this is the event's full
    /// identity, e.g. for `Curated{server, event_id, ..}` selections.
    pub event_id: String,
    /// User-facing name (e.g. "New starred email").
    pub display_name: String,
    /// One-line description shown under the event in a picker.
    pub description: String,
    /// Raw MCP tool called each poll — see
    /// [`ConnectorPollSpec::tool_name`] for the exact calling convention.
    pub tool_name: String,
    /// Default `arguments` object passed to `tool_name`. Always a JSON
    /// object (never a scalar/array) so friendly-param overrides can merge
    /// into it by key.
    pub default_arguments: Value,
    /// The friendly params this event exposes, if any. Empty is valid — not
    /// every curated event needs a user-adjustable knob.
    pub friendly_params: Vec<CuratedFriendlyParam>,
    /// Dot-path cursor, hand-verified to resolve against a real result from
    /// `tool_name`. Curated events never leave this blank — that's the whole
    /// point of curation: a verified cursor that will actually fire.
    pub cursor_path: String,
}

impl CuratedEvent {
    /// The default value for a friendly param, read from `default_arguments`
    /// (the single source of truth — [`CuratedFriendlyParam`] intentionally
    /// carries no separate `default` field to override so the two can never
    /// drift apart).
    pub fn friendly_param_default(&self, arg_key: &str) -> Option<&Value> {
        self.default_arguments.get(arg_key)
    }
}

/// A connector's curated events, keyed by the connector's `server_name`
/// (the same id used for `AssignmentTrigger::ConnectorEvent::server_name` —
/// i.e. whatever the user named the MCP server when they added it).
#[derive(Debug, Clone, PartialEq)]
pub struct CuratedConnector {
    /// MCP server id this connector's events apply to (e.g. `"gmail"`).
    pub server_name: String,
    /// User-facing connector name (e.g. "Gmail").
    pub display_name: String,
    /// Named events curated for this connector.
    pub events: Vec<CuratedEvent>,
}

/// Errors resolving or compiling a curated event.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CuratedEventError {
    #[error("no curated connector named {0:?}")]
    UnknownConnector(String),
    #[error("connector {server_name:?} has no curated event {event_id:?}")]
    UnknownEvent { server_name: String, event_id: String },
    /// A friendly-param override named a key the event never declared. Held
    /// deliberately: silently merging arbitrary keys into `arguments` would
    /// turn the curated tier into an undeclared raw-arguments bypass.
    #[error(
        "event {event_id:?} on connector {server_name:?} has no friendly param {key:?}"
    )]
    UnknownFriendlyParam {
        server_name: String,
        event_id: String,
        key: String,
    },
}

fn args(pairs: &[(&str, Value)]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in pairs {
        map.insert((*k).to_string(), v.clone());
    }
    Value::Object(map)
}

/// The full curated registry. Rebuilt fresh on every call (the data is a
/// small static literal, not something read off disk — see
/// [`crate::plugin_catalog`] for the analogous "logical view" naming
/// convention this module follows).
pub fn catalog() -> Vec<CuratedConnector> {
    vec![
        CuratedConnector {
            server_name: "gmail".to_string(),
            display_name: "Gmail".to_string(),
            events: vec![
                CuratedEvent {
                    event_id: "new_email".to_string(),
                    display_name: "New email".to_string(),
                    description: "Fires when a new email lands in the watched label."
                        .to_string(),
                    tool_name: "list_emails".to_string(),
                    default_arguments: args(&[
                        ("label", Value::String("INBOX".to_string())),
                        ("max_results", Value::from(10)),
                    ]),
                    friendly_params: vec![
                        CuratedFriendlyParam {
                            arg_key: "label".to_string(),
                            label: "Label".to_string(),
                            description:
                                "Gmail label to watch, e.g. INBOX or a custom label name."
                                    .to_string(),
                        },
                        CuratedFriendlyParam {
                            arg_key: "max_results".to_string(),
                            label: "Max results per poll".to_string(),
                            description: "How many recent messages to fetch each poll."
                                .to_string(),
                        },
                    ],
                    cursor_path: "structuredContent.latest_id".to_string(),
                },
                CuratedEvent {
                    event_id: "new_starred_email".to_string(),
                    display_name: "New starred email".to_string(),
                    description: "Fires when a new message is starred.".to_string(),
                    tool_name: "list_starred".to_string(),
                    default_arguments: args(&[("max_results", Value::from(5))]),
                    friendly_params: vec![CuratedFriendlyParam {
                        arg_key: "max_results".to_string(),
                        label: "Max results per poll".to_string(),
                        description: "How many recent starred messages to fetch each poll."
                            .to_string(),
                    }],
                    cursor_path: "structuredContent.latest_id".to_string(),
                },
            ],
        },
        CuratedConnector {
            server_name: "github".to_string(),
            display_name: "GitHub".to_string(),
            events: vec![CuratedEvent {
                event_id: "new_issue".to_string(),
                display_name: "New matching issue".to_string(),
                description: "Fires when a new issue matches the watched search query."
                    .to_string(),
                tool_name: "search_issues".to_string(),
                default_arguments: args(&[
                    ("query", Value::String("is:issue is:open".to_string())),
                    ("max_results", Value::from(10)),
                ]),
                friendly_params: vec![CuratedFriendlyParam {
                    arg_key: "query".to_string(),
                    label: "Search query".to_string(),
                    description: "GitHub issue search query to watch, e.g. \"is:issue is:open label:bug\"."
                        .to_string(),
                }],
                cursor_path: "structuredContent.latest_id".to_string(),
            }],
        },
    ]
}

/// Resolve a single connector's curated events by `server_name`. `None` if
/// no curated entry exists for that connector (the agent-driven watch tier
/// is the fallback for any connector the registry doesn't cover yet).
pub fn connector(server_name: &str) -> Option<CuratedConnector> {
    catalog().into_iter().find(|c| c.server_name == server_name)
}

/// Resolve a single curated event by connector + event id.
pub fn event(server_name: &str, event_id: &str) -> Option<CuratedEvent> {
    connector(server_name)?
        .events
        .into_iter()
        .find(|e| e.event_id == event_id)
}

/// Compile a curated event selection down to the exact [`ConnectorPollSpec`]
/// the deterministic poller already knows how to run: `default_arguments`
/// merged with any declared friendly-param overrides.
///
/// This is the surface a one-shot test-poll endpoint calls directly (it only
/// needs the tool/args/cursor triple to run one live poll, not a full
/// trigger/interval).
///
/// Rejects (rather than silently accepting) an override key that isn't one
/// of the event's declared `friendly_params` — that's the line between
/// "curated with a couple of friendly knobs" and "raw arguments in
/// disguise".
pub fn compile_poll_spec(
    server_name: &str,
    event_id: &str,
    friendly_param_overrides: &HashMap<String, Value>,
) -> Result<ConnectorPollSpec, CuratedEventError> {
    let Some(connector) = connector(server_name) else {
        return Err(CuratedEventError::UnknownConnector(server_name.to_string()));
    };
    let Some(curated) = connector.events.into_iter().find(|e| e.event_id == event_id) else {
        return Err(CuratedEventError::UnknownEvent {
            server_name: server_name.to_string(),
            event_id: event_id.to_string(),
        });
    };

    let mut arguments = curated.default_arguments.clone();
    let obj = arguments
        .as_object_mut()
        .expect("curated default_arguments is always a JSON object");
    for (key, value) in friendly_param_overrides {
        if !curated.friendly_params.iter().any(|p| &p.arg_key == key) {
            return Err(CuratedEventError::UnknownFriendlyParam {
                server_name: server_name.to_string(),
                event_id: event_id.to_string(),
                key: key.clone(),
            });
        }
        obj.insert(key.clone(), value.clone());
    }

    Ok(ConnectorPollSpec {
        tool_name: curated.tool_name,
        arguments,
        cursor_path: Some(curated.cursor_path),
    })
}

/// Compile a curated event selection all the way down to the wire trigger
/// shape (`AssignmentTrigger::ConnectorEvent`) the assignment store
/// persists and `schedule_runner` polls unchanged. `poll_interval_secs` is
/// the caller's own choice (surfaced as a cost governor, not something the
/// registry curates).
pub fn compile_trigger(
    server_name: &str,
    event_id: &str,
    friendly_param_overrides: &HashMap<String, Value>,
    poll_interval_secs: u64,
) -> Result<AssignmentTrigger, CuratedEventError> {
    let poll = compile_poll_spec(server_name, event_id, friendly_param_overrides)?;
    Ok(AssignmentTrigger::ConnectorEvent {
        server_name: server_name.to_string(),
        poll,
        poll_interval_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_resolves_gmail_connector_with_expected_events() {
        let gmail = connector("gmail").expect("gmail is curated");
        assert_eq!(gmail.display_name, "Gmail");
        let ids: Vec<&str> = gmail.events.iter().map(|e| e.event_id.as_str()).collect();
        assert_eq!(ids, vec!["new_email", "new_starred_email"]);
    }

    #[test]
    fn catalog_resolves_github_connector_with_expected_event() {
        let github = connector("github").expect("github is curated");
        assert_eq!(github.events.len(), 1);
        assert_eq!(github.events[0].event_id, "new_issue");
        assert_eq!(github.events[0].tool_name, "search_issues");
    }

    #[test]
    fn unknown_connector_resolves_to_none() {
        assert!(connector("not-a-real-connector").is_none());
    }

    #[test]
    fn unknown_event_resolves_to_none() {
        assert!(event("gmail", "not-a-real-event").is_none());
    }

    #[test]
    fn every_catalog_event_has_a_non_blank_cursor_path() {
        // Curated events are hand-verified to resolve — they must never hit
        // the blank-cursor no-fire gap documented on
        // `ConnectorPollSpec::cursor_path`.
        for connector in catalog() {
            for event in connector.events {
                assert!(
                    !event.cursor_path.trim().is_empty(),
                    "{}/{} has a blank cursor_path",
                    connector.server_name,
                    event.event_id
                );
            }
        }
    }

    #[test]
    fn friendly_param_default_reads_from_default_arguments() {
        let ev = event("gmail", "new_email").unwrap();
        assert_eq!(
            ev.friendly_param_default("label"),
            Some(&Value::String("INBOX".to_string()))
        );
        assert_eq!(ev.friendly_param_default("not_a_param"), None);
    }

    #[test]
    fn compile_new_starred_email_matches_the_hand_written_fixture() {
        // Parity check against the exact fixture already exercised in
        // ao_protocol::assignment's own tests
        // (`sample_connector_event_assignment`) — proves the curated entry
        // compiles to precisely the shape the poller already runs today.
        let trigger = compile_trigger("gmail", "new_starred_email", &HashMap::new(), 300)
            .expect("compiles");
        assert_eq!(
            trigger,
            AssignmentTrigger::ConnectorEvent {
                server_name: "gmail".to_string(),
                poll: ConnectorPollSpec {
                    tool_name: "list_starred".to_string(),
                    arguments: serde_json::json!({ "max_results": 5 }),
                    cursor_path: Some("structuredContent.latest_id".to_string()),
                },
                poll_interval_secs: 300,
            }
        );
    }

    #[test]
    fn compile_new_starred_email_with_max_results_override() {
        let mut overrides = HashMap::new();
        overrides.insert("max_results".to_string(), Value::from(20));
        let spec = compile_poll_spec("gmail", "new_starred_email", &overrides).expect("compiles");
        assert_eq!(spec.arguments, serde_json::json!({ "max_results": 20 }));
        assert_eq!(spec.tool_name, "list_starred");
        assert_eq!(spec.cursor_path.as_deref(), Some("structuredContent.latest_id"));
    }

    #[test]
    fn compile_new_email_default_arguments() {
        let spec = compile_poll_spec("gmail", "new_email", &HashMap::new()).expect("compiles");
        assert_eq!(spec.tool_name, "list_emails");
        assert_eq!(
            spec.arguments,
            serde_json::json!({ "label": "INBOX", "max_results": 10 })
        );
        assert_eq!(spec.cursor_path.as_deref(), Some("structuredContent.latest_id"));
    }

    #[test]
    fn compile_new_email_with_label_override() {
        let mut overrides = HashMap::new();
        overrides.insert("label".to_string(), Value::String("Newsletters".to_string()));
        let spec = compile_poll_spec("gmail", "new_email", &overrides).expect("compiles");
        assert_eq!(
            spec.arguments,
            serde_json::json!({ "label": "Newsletters", "max_results": 10 })
        );
    }

    #[test]
    fn compile_github_new_issue_default_and_override() {
        let spec = compile_poll_spec("github", "new_issue", &HashMap::new()).expect("compiles");
        assert_eq!(spec.tool_name, "search_issues");
        assert_eq!(
            spec.arguments,
            serde_json::json!({ "query": "is:issue is:open", "max_results": 10 })
        );

        let mut overrides = HashMap::new();
        overrides.insert(
            "query".to_string(),
            Value::String("is:issue is:open label:bug".to_string()),
        );
        let overridden = compile_poll_spec("github", "new_issue", &overrides).expect("compiles");
        assert_eq!(
            overridden.arguments,
            serde_json::json!({ "query": "is:issue is:open label:bug", "max_results": 10 })
        );
    }

    #[test]
    fn compile_rejects_unknown_connector() {
        let err = compile_poll_spec("not-a-connector", "new_email", &HashMap::new()).unwrap_err();
        assert_eq!(
            err,
            CuratedEventError::UnknownConnector("not-a-connector".to_string())
        );
    }

    #[test]
    fn compile_rejects_unknown_event() {
        let err = compile_poll_spec("gmail", "not-an-event", &HashMap::new()).unwrap_err();
        assert_eq!(
            err,
            CuratedEventError::UnknownEvent {
                server_name: "gmail".to_string(),
                event_id: "not-an-event".to_string(),
            }
        );
    }

    #[test]
    fn compile_rejects_unknown_friendly_param_override() {
        let mut overrides = HashMap::new();
        overrides.insert("arbitrary_raw_key".to_string(), Value::from(true));
        let err = compile_poll_spec("gmail", "new_starred_email", &overrides).unwrap_err();
        assert_eq!(
            err,
            CuratedEventError::UnknownFriendlyParam {
                server_name: "gmail".to_string(),
                event_id: "new_starred_email".to_string(),
                key: "arbitrary_raw_key".to_string(),
            }
        );
    }

    #[test]
    fn compile_trigger_threads_server_name_and_poll_interval() {
        let trigger = compile_trigger("github", "new_issue", &HashMap::new(), 900).unwrap();
        let AssignmentTrigger::ConnectorEvent {
            server_name,
            poll_interval_secs,
            ..
        } = trigger
        else {
            panic!("expected ConnectorEvent");
        };
        assert_eq!(server_name, "github");
        assert_eq!(poll_interval_secs, 900);
    }
}
