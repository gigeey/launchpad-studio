//! Slack Socket Mode wire format: envelope parsing for the five envelope
//! `type`s Socket Mode sends (`hello`, `events_api`, `disconnect`,
//! `slash_commands`, `interactive`), the two inner Events API event types
//! this transport acts on (`message`, `app_mention`), the outbound envelope
//! acknowledgement Slack requires within 3 seconds of delivery, and a
//! disconnect-reason classifier distinguishing a routine
//! hourly socket refresh from a hard failure.
//!
//! Everything here is pure — no socket, no clock — so [`parse_envelope`],
//! [`classify_disconnect`], and [`Acknowledge`]'s serialization are
//! exhaustively unit-testable without a live Socket Mode connection.
//! [`super::socket_seam`] is the only place a raw frame is actually read off
//! (or written to) a socket; it hands this module the frame's raw text, and
//! this module turns it into a [`SocketModeEvent`] — and, going the other
//! way, turns an [`Acknowledge`] into the JSON text the seam sends back.
//!
//! `envelope_id` and the inner event's `event_id` are easy to conflate —
//! both are opaque Slack-minted strings sitting a few fields apart in the
//! same payload — but they answer different questions. `envelope_id` acks
//! *delivery of this envelope* (the ack dies at the ingress boundary, never
//! reaches dispatch). `event_id` dedupes *the inner event*, independently,
//! against redelivery after a slow ack or a reconnect. [`SocketModeEvent::EventsApi`]
//! and [`EventsApiPayload`] keep both reachable, separately, on purpose.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("socket mode frame was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0} envelope carried no envelope_id")]
    MissingEnvelopeId(&'static str),
    #[error("events_api envelope carried no payload")]
    MissingPayload,
}

/// One parsed Socket Mode envelope, reduced to what this transport acts on.
/// Mirrors [`crate::channels::discord::protocol::GatewayEvent`]'s role for
/// the Gateway transport.
#[derive(Debug, Clone, PartialEq)]
pub enum SocketModeEvent {
    /// Sent once, right after the socket opens. Carries no fields this
    /// transport needs.
    Hello,
    /// The one envelope type this transport actually dispatches on. See the
    /// module doc for why `envelope_id` and `event.event_id` are kept as
    /// two separate fields rather than one.
    EventsApi { envelope_id: String, event: EventsApiPayload },
    /// Slack rotating (or force-closing) this socket. See
    /// [`classify_disconnect`] for what a caller should do about it.
    Disconnect { reason: DisconnectReason },
    /// A slash command invocation. Parsed only far enough to ack —
    /// dispatching slash commands is out of scope until a future phase
    /// (`interactive`/`slash_commands` are parse-and-ignore for now).
    SlashCommands { envelope_id: String },
    /// A Block Kit interactive action. Same parse-and-ignore scope as
    /// [`Self::SlashCommands`].
    Interactive { envelope_id: String },
    /// An envelope `type` Slack sent that this transport doesn't (yet) act
    /// on. Never fatal, mirroring
    /// [`crate::channels::discord::protocol::GatewayEvent::Unknown`] —
    /// Socket Mode can grow new envelope types over time and a connection
    /// should stay open rather than error out on one.
    Unknown,
}

/// The parsed `events_api` envelope's payload: the inner event's own
/// dedup id, plus the event itself.
#[derive(Debug, Clone, PartialEq)]
pub struct EventsApiPayload {
    /// Dedups the inner event against redelivery — never used to ack (that's
    /// the outer envelope's `envelope_id`).
    pub event_id: String,
    pub event: SlackEvent,
}

/// One inner Events API event type this transport recognizes by its own
/// `type` field. Any other inner event type still round-trips as
/// [`Self::Other`] rather than failing the whole envelope parse.
#[derive(Debug, Clone, PartialEq)]
pub enum SlackEvent {
    Message(SlackMessageEvent),
    AppMention(SlackMessageEvent),
    Other,
}

/// The subset of Slack's `message` / `app_mention` event payload this
/// transport needs. Both event types share this shape.
///
/// `ts` and `thread_ts` are opaque Slack message-timestamp strings (e.g.
/// `"1701234567.123456"`), **never** numbers.
/// `thread_ts` is `None` on a top-level, non-threaded message; `bot_id` and
/// `subtype` are `None` on a plain human message.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SlackMessageEvent {
    pub channel: String,
    /// Absent on most bot-authored messages (which carry `bot_id` instead).
    #[serde(default)]
    pub user: String,
    pub ts: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub team: String,
}

/// The three documented `disconnect` envelope `reason` values,
/// plus [`Self::Other`] so an undocumented future value still parses instead
/// of erroring — [`classify_disconnect`] treats it cautiously as
/// [`DisconnectSeverity::Warning`] rather than assuming it's routine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Slack's routine ~hourly socket rotation. Expected;
    /// reconnect quietly.
    RefreshRequested,
    /// Soft signal that the connection will be closed soon. Reconnect.
    Warning,
    /// The app-level token itself was revoked/disabled. Reconnecting with
    /// the same token will just fail again — do not blindly retry.
    LinkDisabled,
    Other(String),
}

/// How a caller should react to a [`DisconnectReason`] — the classification
/// the runner gate ("on `disconnect`, open the NEW connection *before*
/// closing the old") depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectSeverity {
    /// Expected rotation — reconnect quietly, no warning logged.
    RoutineRefresh,
    /// Reconnect, but it's worth a log line: something Slack-side is amiss.
    Warning,
    /// Reconnecting with the same credentials will not help. Surface this
    /// to the user rather than looping a reconnect attempt forever.
    HardError,
}

/// Classifies a [`DisconnectReason`] into what a reconnect loop should do
/// about it. See [`DisconnectSeverity`] for what each tier means.
pub fn classify_disconnect(reason: &DisconnectReason) -> DisconnectSeverity {
    match reason {
        DisconnectReason::RefreshRequested => DisconnectSeverity::RoutineRefresh,
        DisconnectReason::Warning => DisconnectSeverity::Warning,
        DisconnectReason::LinkDisabled => DisconnectSeverity::HardError,
        // An undocumented reason is unknown, not "safe" — treat it the same
        // as an explicit warning rather than assuming it's routine.
        DisconnectReason::Other(_) => DisconnectSeverity::Warning,
    }
}

/// The outbound envelope acknowledgement: Slack requires this sent back
/// on the socket within 3 seconds of any acked envelope arriving, ack
/// semantics dying at the ingress boundary before dispatch ever runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Acknowledge {
    pub envelope_id: String,
}

impl Acknowledge {
    pub fn new(envelope_id: impl Into<String>) -> Self {
        Self { envelope_id: envelope_id.into() }
    }

    /// The exact JSON Slack expects back on the socket: `{"envelope_id":"..."}`,
    /// nothing else.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("Acknowledge carries only a String field and always serializes")
    }
}

/// The outer envelope every Socket Mode frame carries. `payload` is left as
/// a raw [`serde_json::Value`] here since its shape depends entirely on
/// `type`.
#[derive(Deserialize)]
struct RawEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct RawEventsApiPayload {
    event_id: String,
    event: serde_json::Value,
}

/// Parses one raw Socket Mode text frame into a [`SocketModeEvent`]. The
/// only network-independent step in the whole inbound path — everything
/// else (the socket read itself, `apps.connections.open`) lives behind
/// [`super::socket_seam::SlackSocketSeam`].
pub fn parse_envelope(raw: &str) -> Result<SocketModeEvent, ProtocolError> {
    let envelope: RawEnvelope = serde_json::from_str(raw)?;

    let event = match envelope.kind.as_str() {
        "hello" => SocketModeEvent::Hello,
        "events_api" => {
            let envelope_id = envelope.envelope_id.ok_or(ProtocolError::MissingEnvelopeId("events_api"))?;
            let payload = envelope.payload.ok_or(ProtocolError::MissingPayload)?;
            let raw_payload: RawEventsApiPayload = serde_json::from_value(payload)?;

            let event_type = raw_payload.event.get("type").and_then(|v| v.as_str()).unwrap_or_default();
            let event = match event_type {
                "message" => SlackEvent::Message(serde_json::from_value(raw_payload.event)?),
                "app_mention" => SlackEvent::AppMention(serde_json::from_value(raw_payload.event)?),
                _ => SlackEvent::Other,
            };

            SocketModeEvent::EventsApi {
                envelope_id,
                event: EventsApiPayload { event_id: raw_payload.event_id, event },
            }
        }
        "disconnect" => {
            let reason = parse_disconnect_reason(envelope.reason.as_deref().unwrap_or_default());
            SocketModeEvent::Disconnect { reason }
        }
        "slash_commands" => {
            let envelope_id = envelope.envelope_id.ok_or(ProtocolError::MissingEnvelopeId("slash_commands"))?;
            SocketModeEvent::SlashCommands { envelope_id }
        }
        "interactive" => {
            let envelope_id = envelope.envelope_id.ok_or(ProtocolError::MissingEnvelopeId("interactive"))?;
            SocketModeEvent::Interactive { envelope_id }
        }
        _ => SocketModeEvent::Unknown,
    };
    Ok(event)
}

fn parse_disconnect_reason(reason: &str) -> DisconnectReason {
    match reason {
        "refresh_requested" => DisconnectReason::RefreshRequested,
        "warning" => DisconnectReason::Warning,
        "link_disabled" => DisconnectReason::LinkDisabled,
        other => DisconnectReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Envelope parsing ---

    #[test]
    fn parses_hello() {
        let raw = r#"{"type":"hello","num_connections":1,"debug_info":{"host":"applink-1"},"connection_info":{"app_id":"A0123"}}"#;
        assert_eq!(parse_envelope(raw).unwrap(), SocketModeEvent::Hello);
    }

    #[test]
    fn parses_a_non_threaded_message_event() {
        let raw = r#"{
            "envelope_id":"6a77-envelope",
            "payload":{
                "token":"verification","team_id":"T111","api_app_id":"A111",
                "event":{
                    "type":"message","channel":"C123","user":"U456",
                    "text":"hello there","ts":"1701234567.000100","team":"T111"
                },
                "type":"event_callback","event_id":"Ev001","event_time":1701234567
            },
            "type":"events_api","accepts_response_payload":false
        }"#;

        let event = parse_envelope(raw).unwrap();
        let SocketModeEvent::EventsApi { envelope_id, event: EventsApiPayload { event_id, event } } = event else {
            panic!("expected an EventsApi envelope");
        };
        assert_eq!(envelope_id, "6a77-envelope");
        assert_eq!(event_id, "Ev001");
        let SlackEvent::Message(msg) = event else {
            panic!("expected a Message event");
        };
        assert_eq!(msg.channel, "C123");
        assert_eq!(msg.user, "U456");
        assert_eq!(msg.ts, "1701234567.000100");
        assert_eq!(msg.thread_ts, None, "a top-level message has no thread_ts");
        assert_eq!(msg.text, "hello there");
        assert_eq!(msg.team, "T111");
        assert_eq!(msg.bot_id, None);
        assert_eq!(msg.subtype, None);
    }

    #[test]
    fn parses_a_threaded_message_event_with_thread_ts_distinct_from_ts() {
        let raw = r#"{
            "envelope_id":"env-threaded",
            "payload":{
                "event":{
                    "type":"message","channel":"C123","user":"U456",
                    "text":"reply in thread","ts":"1701234600.000200",
                    "thread_ts":"1701234567.000100","team":"T111"
                },
                "type":"event_callback","event_id":"Ev002"
            },
            "type":"events_api"
        }"#;

        let event = parse_envelope(raw).unwrap();
        let SocketModeEvent::EventsApi { event: EventsApiPayload { event, .. }, .. } = event else {
            panic!("expected an EventsApi envelope");
        };
        let SlackEvent::Message(msg) = event else {
            panic!("expected a Message event");
        };
        assert_eq!(msg.ts, "1701234600.000200");
        assert_eq!(msg.thread_ts.as_deref(), Some("1701234567.000100"));
        assert_ne!(Some(msg.ts.as_str()), msg.thread_ts.as_deref(), "ts and thread_ts must be distinct strings");
    }

    #[test]
    fn parses_a_bot_authored_message_with_bot_id_and_subtype() {
        let raw = r#"{
            "envelope_id":"env-bot",
            "payload":{
                "event":{
                    "type":"message","channel":"C123",
                    "text":"a bot posted this","ts":"1701234700.000300",
                    "subtype":"bot_message","bot_id":"B999","team":"T111"
                },
                "type":"event_callback","event_id":"Ev003"
            },
            "type":"events_api"
        }"#;

        let event = parse_envelope(raw).unwrap();
        let SocketModeEvent::EventsApi { event: EventsApiPayload { event, .. }, .. } = event else {
            panic!("expected an EventsApi envelope");
        };
        let SlackEvent::Message(msg) = event else {
            panic!("expected a Message event");
        };
        assert_eq!(msg.bot_id.as_deref(), Some("B999"));
        assert_eq!(msg.subtype.as_deref(), Some("bot_message"));
        assert_eq!(msg.user, "", "bot messages default an absent user to empty, not an error");
    }

    #[test]
    fn parses_an_app_mention_event() {
        let raw = r#"{
            "envelope_id":"env-mention",
            "payload":{
                "event":{
                    "type":"app_mention","channel":"C123","user":"U456",
                    "text":"<@U0BOT> is it Friday yet?","ts":"1701234800.000400","team":"T111"
                },
                "type":"event_callback","event_id":"Ev004"
            },
            "type":"events_api"
        }"#;

        let event = parse_envelope(raw).unwrap();
        let SocketModeEvent::EventsApi { event: EventsApiPayload { event, .. }, .. } = event else {
            panic!("expected an EventsApi envelope");
        };
        let SlackEvent::AppMention(msg) = event else {
            panic!("expected an AppMention event");
        };
        assert_eq!(msg.user, "U456");
        assert_eq!(msg.text, "<@U0BOT> is it Friday yet?");
    }

    #[test]
    fn envelope_id_and_event_id_are_extracted_as_different_values() {
        let raw = r#"{
            "envelope_id":"envelope-outer-value",
            "payload":{
                "event":{"type":"message","channel":"C1","user":"U1","text":"hi","ts":"1.1"},
                "type":"event_callback","event_id":"event-inner-value"
            },
            "type":"events_api"
        }"#;

        let SocketModeEvent::EventsApi { envelope_id, event: EventsApiPayload { event_id, .. } } =
            parse_envelope(raw).unwrap()
        else {
            panic!("expected an EventsApi envelope");
        };
        assert_eq!(envelope_id, "envelope-outer-value");
        assert_eq!(event_id, "event-inner-value");
        assert_ne!(envelope_id, event_id, "envelope_id acks; event_id dedups — they must never be conflated");
    }

    #[test]
    fn parses_disconnect_for_each_documented_reason() {
        for (raw_reason, expected) in [
            (r#"{"type":"disconnect","reason":"refresh_requested"}"#, DisconnectReason::RefreshRequested),
            (r#"{"type":"disconnect","reason":"warning"}"#, DisconnectReason::Warning),
            (r#"{"type":"disconnect","reason":"link_disabled"}"#, DisconnectReason::LinkDisabled),
        ] {
            assert_eq!(parse_envelope(raw_reason).unwrap(), SocketModeEvent::Disconnect { reason: expected });
        }
    }

    #[test]
    fn parses_slash_commands_and_interactive_far_enough_to_ack() {
        assert_eq!(
            parse_envelope(r#"{"envelope_id":"sc-1","payload":{},"type":"slash_commands"}"#).unwrap(),
            SocketModeEvent::SlashCommands { envelope_id: "sc-1".to_string() }
        );
        assert_eq!(
            parse_envelope(r#"{"envelope_id":"ia-1","payload":{},"type":"interactive"}"#).unwrap(),
            SocketModeEvent::Interactive { envelope_id: "ia-1".to_string() }
        );
    }

    #[test]
    fn an_unrecognized_envelope_type_is_unknown_not_an_error() {
        let raw = r#"{"type":"some_future_envelope_type"}"#;
        assert_eq!(parse_envelope(raw).unwrap(), SocketModeEvent::Unknown);
    }

    #[test]
    fn an_unrecognized_inner_event_type_is_other_not_an_error() {
        let raw = r#"{
            "envelope_id":"env-other",
            "payload":{"event":{"type":"reaction_added"},"type":"event_callback","event_id":"Ev999"},
            "type":"events_api"
        }"#;
        let SocketModeEvent::EventsApi { event: EventsApiPayload { event, .. }, .. } = parse_envelope(raw).unwrap()
        else {
            panic!("expected an EventsApi envelope");
        };
        assert_eq!(event, SlackEvent::Other);
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        assert!(parse_envelope("not json").is_err());
    }

    #[test]
    fn events_api_without_an_envelope_id_is_a_parse_error() {
        let raw = r#"{"payload":{"event":{"type":"message"},"event_id":"Ev1"},"type":"events_api"}"#;
        assert!(matches!(parse_envelope(raw), Err(ProtocolError::MissingEnvelopeId("events_api"))));
    }

    // --- Disconnect classification ---

    #[test]
    fn classifies_refresh_requested_as_routine() {
        assert_eq!(classify_disconnect(&DisconnectReason::RefreshRequested), DisconnectSeverity::RoutineRefresh);
    }

    #[test]
    fn classifies_warning_as_warning() {
        assert_eq!(classify_disconnect(&DisconnectReason::Warning), DisconnectSeverity::Warning);
    }

    #[test]
    fn classifies_link_disabled_as_hard_error() {
        assert_eq!(classify_disconnect(&DisconnectReason::LinkDisabled), DisconnectSeverity::HardError);
    }

    #[test]
    fn classifies_an_unrecognized_reason_cautiously_as_warning_not_routine() {
        assert_eq!(
            classify_disconnect(&DisconnectReason::Other("some_future_reason".to_string())),
            DisconnectSeverity::Warning
        );
    }

    // --- Acknowledge ---

    #[test]
    fn acknowledge_serializes_to_the_exact_shape_slack_expects() {
        let ack = Acknowledge::new("abc-123");
        assert_eq!(ack.to_json(), r#"{"envelope_id":"abc-123"}"#);
    }
}
