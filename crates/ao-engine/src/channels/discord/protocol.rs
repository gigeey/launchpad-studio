//! Discord Gateway v10 wire format: opcode envelope parsing, the handful of
//! dispatch payloads this transport acts on, outbound payload construction
//! (IDENTIFY/RESUME/heartbeat), intent-bit computation, and close-code
//! classification.
//!
//! Everything here is pure — no socket, no clock reads beyond what a caller
//! passes in — so [`parse_gateway_payload`], [`compute_intents`], and
//! [`is_resumable_close_code`] are exhaustively unit-testable without a live
//! gateway connection. [`super::gateway_seam`] is the only place a raw frame
//! actually gets read off (or written to) a socket; it calls into this
//! module to turn bytes into a [`GatewayEvent`] and back.

use serde::Deserialize;
use thiserror::Error;

/// Default Gateway v10 endpoint, used for a fresh `IDENTIFY`. A `RESUME`
/// instead reconnects to the `resume_gateway_url` a prior `READY` handed
/// back — see [`GatewayEvent::Dispatch`]'s `Ready` variant.
pub const DEFAULT_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

// --- Gateway opcodes (op10 HELLO, op1 HEARTBEAT, op2 IDENTIFY, op6 RESUME,
// op7 RECONNECT, op9 INVALID_SESSION, op11 HEARTBEAT_ACK, op0 DISPATCH) ---
const OP_DISPATCH: u8 = 0;
const OP_HEARTBEAT: u8 = 1;
const OP_RECONNECT: u8 = 7;
const OP_INVALID_SESSION: u8 = 9;
const OP_HELLO: u8 = 10;
const OP_HEARTBEAT_ACK: u8 = 11;

// --- Gateway intent bits this transport ever requests. GUILD_MEMBERS is
// privileged: Discord fails the login outright if a bot requests it without
// having the intent approved in the developer portal, which is why
// `needs_members_intent` (see `super::security`) only asks for it when the
// binding's own config actually needs member/role resolution. ---
const INTENT_GUILD_MEMBERS: u32 = 1 << 1;
const INTENT_GUILD_MESSAGES: u32 = 1 << 9;
const INTENT_DIRECT_MESSAGES: u32 = 1 << 12;
const INTENT_MESSAGE_CONTENT: u32 = 1 << 15;

/// Close codes Discord documents as *not* safe to resume a session after —
/// the client must clear its session and send a fresh `IDENTIFY` instead of
/// `RESUME`. Every other code (including plain WebSocket closes with no
/// Discord-specific code at all) is treated as resumable: worst case,
/// Discord itself rejects the `RESUME` with `op9 InvalidSession { resumable:
/// false }` and the caller falls back to identifying fresh from there, so
/// erring toward "try resume" here never gets stuck.
const NON_RESUMABLE_CLOSE_CODES: &[u16] = &[
    4004, // Authentication failed
    4010, // Invalid shard
    4011, // Sharding required
    4012, // Invalid API version
    4013, // Invalid intent(s)
    4014, // Disallowed intent(s)
];

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("gateway frame was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("dispatch frame carried no sequence number")]
    MissingSequence,
}

/// A parsed Gateway frame, reduced to what this transport acts on.
/// [`GatewayEvent::HeartbeatSent`] is the one variant [`parse_gateway_payload`]
/// never produces — it's synthesized by [`super::gateway_seam`] when its
/// internal heartbeat timer fires, so the caller sees "a heartbeat just went
/// out" as just another event in the same stream instead of a separate
/// side channel.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayEvent {
    Hello { heartbeat_interval_ms: u64 },
    HeartbeatAck,
    /// The server asked for an out-of-cycle heartbeat right now (`op1`
    /// arriving from Discord rather than being sent by us).
    HeartbeatRequest,
    Reconnect,
    InvalidSession { resumable: bool },
    Dispatch { seq: u64, kind: DispatchKind },
    /// An opcode this transport doesn't need to act on (e.g. a future
    /// Gateway addition). Never fatal — the connection stays open.
    Unknown,
    /// Synthesized by the seam, never parsed from a frame — see the type doc.
    HeartbeatSent,
}

/// One `op0` dispatch event this transport recognizes by its `t` field.
/// Any other dispatch type (`GUILD_CREATE`, `PRESENCE_UPDATE`, ...) still
/// advances `s` (via [`GatewayEvent::Dispatch`]'s `seq`) but carries no
/// payload this transport parses further.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchKind {
    Ready { session_id: String, resume_gateway_url: String, own_user_id: String },
    MessageCreate(MessageCreateEvent),
    Resumed,
    Other,
}

/// The subset of Discord's `MESSAGE_CREATE` payload this transport needs:
/// enough to dedup, filter self/bot authors, authorize, and hand the text
/// off to [`crate::channels::submit_inbound_message`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageCreateEvent {
    pub id: String,
    pub channel_id: String,
    /// Absent for a DM — the field's presence, not its value, is what marks
    /// a message as guild-scoped throughout this transport.
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub content: String,
    pub author: MessageAuthor,
    /// The author's partial guild member object — present on every
    /// guild-scoped message, `None` for a DM (a DM has no guild to be a
    /// member of).
    #[serde(default)]
    pub member: Option<MessageMember>,
    /// Users explicitly @-mentioned in the message. Discord sends the full
    /// user object for each; only `id` is needed here.
    #[serde(default)]
    pub mentions: Vec<MentionedUser>,
    /// Whether the message used `@everyone` or `@here` — deliberately kept
    /// separate from [`Self::mentions`] since it must never be treated as a
    /// mention of any specific user, the bot included.
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default)]
    pub mention_roles: Vec<String>,
    /// Present when this message is a reply — points at the message (and
    /// its channel/guild) being replied to.
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageAuthor {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub bot: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageMember {
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MentionedUser {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageReference {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub guild_id: Option<String>,
}

/// The outer `{op, d, s, t}` envelope every Gateway frame carries.
#[derive(Deserialize)]
struct RawEnvelope {
    op: u8,
    #[serde(default)]
    d: serde_json::Value,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

#[derive(Deserialize)]
struct HelloData {
    heartbeat_interval: u64,
}

#[derive(Deserialize)]
struct ReadyData {
    session_id: String,
    resume_gateway_url: String,
    user: ReadyUser,
}

#[derive(Deserialize)]
struct ReadyUser {
    id: String,
}

/// Parses one raw Gateway text frame into a [`GatewayEvent`]. The only
/// network-independent step in the whole inbound path — everything else
/// (the socket read itself, the heartbeat clock) lives behind
/// [`super::gateway_seam::GatewaySeam`].
pub fn parse_gateway_payload(raw: &str) -> Result<GatewayEvent, ProtocolError> {
    let envelope: RawEnvelope = serde_json::from_str(raw)?;

    let event = match envelope.op {
        OP_HELLO => {
            let data: HelloData = serde_json::from_value(envelope.d)?;
            GatewayEvent::Hello { heartbeat_interval_ms: data.heartbeat_interval }
        }
        OP_HEARTBEAT_ACK => GatewayEvent::HeartbeatAck,
        OP_HEARTBEAT => GatewayEvent::HeartbeatRequest,
        OP_RECONNECT => GatewayEvent::Reconnect,
        OP_INVALID_SESSION => {
            let resumable = envelope.d.as_bool().unwrap_or(false);
            GatewayEvent::InvalidSession { resumable }
        }
        OP_DISPATCH => {
            let seq = envelope.s.ok_or(ProtocolError::MissingSequence)?;
            let kind = match envelope.t.as_deref() {
                Some("READY") => {
                    let data: ReadyData = serde_json::from_value(envelope.d)?;
                    DispatchKind::Ready {
                        session_id: data.session_id,
                        resume_gateway_url: data.resume_gateway_url,
                        own_user_id: data.user.id,
                    }
                }
                Some("MESSAGE_CREATE") => {
                    let data: MessageCreateEvent = serde_json::from_value(envelope.d)?;
                    DispatchKind::MessageCreate(data)
                }
                Some("RESUMED") => DispatchKind::Resumed,
                _ => DispatchKind::Other,
            };
            GatewayEvent::Dispatch { seq, kind }
        }
        _ => GatewayEvent::Unknown,
    };
    Ok(event)
}

/// The `intents` bitmask this transport always requests
/// (`GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT`), plus the
/// privileged `GUILD_MEMBERS` bit when `include_guild_members` is set — see
/// [`super::security::needs_members_intent`] for how that's decided.
pub fn compute_intents(include_guild_members: bool) -> u32 {
    let base = INTENT_GUILD_MESSAGES | INTENT_DIRECT_MESSAGES | INTENT_MESSAGE_CONTENT;
    if include_guild_members {
        base | INTENT_GUILD_MEMBERS
    } else {
        base
    }
}

/// `op2 IDENTIFY`, sent once per fresh (non-resumed) connection.
pub fn identify_payload(token: &str, intents: u32) -> serde_json::Value {
    serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents,
            "properties": {
                "os": std::env::consts::OS,
                "browser": "launchpad_studio",
                "device": "launchpad_studio",
            }
        }
    })
}

/// `op6 RESUME`, sent instead of `IDENTIFY` when reconnecting with a still-
/// live `session_id` after a resumable close.
pub fn resume_payload(token: &str, session_id: &str, seq: u64) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "token": token,
            "session_id": session_id,
            "seq": seq,
        }
    })
}

/// `op1 HEARTBEAT`, carrying the last dispatch sequence seen (or `null`
/// before any dispatch has arrived).
pub fn heartbeat_payload(last_seq: Option<u64>) -> serde_json::Value {
    serde_json::json!({ "op": 1, "d": last_seq })
}

/// Whether a Gateway-documented WebSocket close `code` still permits a
/// `RESUME` on reconnect. See [`NON_RESUMABLE_CLOSE_CODES`] for the
/// documented exceptions and why any other code (including an unrecognized
/// one) defaults to resumable.
pub fn is_resumable_close_code(code: u16) -> bool {
    !NON_RESUMABLE_CLOSE_CODES.contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Intent computation ---

    #[test]
    fn compute_intents_always_includes_the_base_three() {
        let intents = compute_intents(false);
        assert_eq!(intents & INTENT_GUILD_MESSAGES, INTENT_GUILD_MESSAGES);
        assert_eq!(intents & INTENT_DIRECT_MESSAGES, INTENT_DIRECT_MESSAGES);
        assert_eq!(intents & INTENT_MESSAGE_CONTENT, INTENT_MESSAGE_CONTENT);
        assert_eq!(intents & INTENT_GUILD_MEMBERS, 0, "must not request the privileged intent unasked");
    }

    #[test]
    fn compute_intents_adds_guild_members_only_when_requested() {
        let intents = compute_intents(true);
        assert_eq!(intents & INTENT_GUILD_MEMBERS, INTENT_GUILD_MEMBERS);
    }

    // --- Envelope / dispatch parsing ---

    #[test]
    fn parses_hello() {
        let raw = r#"{"op":10,"d":{"heartbeat_interval":41250}}"#;
        assert_eq!(parse_gateway_payload(raw).unwrap(), GatewayEvent::Hello { heartbeat_interval_ms: 41250 });
    }

    #[test]
    fn parses_heartbeat_ack() {
        let raw = r#"{"op":11,"d":null}"#;
        assert_eq!(parse_gateway_payload(raw).unwrap(), GatewayEvent::HeartbeatAck);
    }

    #[test]
    fn parses_reconnect() {
        let raw = r#"{"op":7,"d":null}"#;
        assert_eq!(parse_gateway_payload(raw).unwrap(), GatewayEvent::Reconnect);
    }

    #[test]
    fn parses_invalid_session_resumable_and_not() {
        assert_eq!(
            parse_gateway_payload(r#"{"op":9,"d":true}"#).unwrap(),
            GatewayEvent::InvalidSession { resumable: true }
        );
        assert_eq!(
            parse_gateway_payload(r#"{"op":9,"d":false}"#).unwrap(),
            GatewayEvent::InvalidSession { resumable: false }
        );
    }

    #[test]
    fn parses_ready_into_session_state() {
        let raw = r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://resume.example/","user":{"id":"777"}}}"#;
        let event = parse_gateway_payload(raw).unwrap();
        assert_eq!(
            event,
            GatewayEvent::Dispatch {
                seq: 1,
                kind: DispatchKind::Ready {
                    session_id: "abc".to_string(),
                    resume_gateway_url: "wss://resume.example/".to_string(),
                    own_user_id: "777".to_string(),
                }
            }
        );
    }

    #[test]
    fn parses_a_guild_message_create_with_inline_member_roles() {
        let raw = r#"{"op":0,"s":42,"t":"MESSAGE_CREATE","d":{
            "id":"999","channel_id":"111","guild_id":"222","content":"hello world",
            "author":{"id":"333","username":"alice","bot":false},
            "member":{"roles":["444","555"]}
        }}"#;
        let event = parse_gateway_payload(raw).unwrap();
        assert_eq!(
            event,
            GatewayEvent::Dispatch {
                seq: 42,
                kind: DispatchKind::MessageCreate(MessageCreateEvent {
                    id: "999".to_string(),
                    channel_id: "111".to_string(),
                    guild_id: Some("222".to_string()),
                    content: "hello world".to_string(),
                    author: MessageAuthor { id: "333".to_string(), username: "alice".to_string(), bot: false },
                    member: Some(MessageMember { roles: vec!["444".to_string(), "555".to_string()] }),
                    mentions: vec![],
                    mention_everyone: false,
                    mention_roles: vec![],
                    message_reference: None,
                })
            }
        );
    }

    #[test]
    fn parses_a_message_create_with_mentions_and_a_message_reference() {
        let raw = r#"{"op":0,"s":42,"t":"MESSAGE_CREATE","d":{
            "id":"999","channel_id":"111","guild_id":"222","content":"<@777> hi",
            "author":{"id":"333","username":"alice","bot":false},
            "member":{"roles":["444"]},
            "mentions":[{"id":"777","username":"bot-user","bot":true}],
            "mention_everyone":false,
            "mention_roles":["888"],
            "message_reference":{"message_id":"1000","channel_id":"111","guild_id":"222"}
        }}"#;
        let event = parse_gateway_payload(raw).unwrap();
        let GatewayEvent::Dispatch { kind: DispatchKind::MessageCreate(msg), .. } = event else {
            panic!("expected a MessageCreate dispatch");
        };
        assert_eq!(msg.mentions, vec![MentionedUser { id: "777".to_string() }]);
        assert!(!msg.mention_everyone);
        assert_eq!(msg.mention_roles, vec!["888".to_string()]);
        assert_eq!(
            msg.message_reference,
            Some(MessageReference {
                message_id: Some("1000".to_string()),
                channel_id: Some("111".to_string()),
                guild_id: Some("222".to_string()),
            })
        );
    }

    #[test]
    fn parses_a_message_create_without_mentions_or_reply_fields_using_defaults() {
        let raw = r#"{"op":0,"s":7,"t":"MESSAGE_CREATE","d":{
            "id":"1","channel_id":"55","guild_id":"66","content":"hi",
            "author":{"id":"9","username":"bob"},
            "member":{"roles":[]}
        }}"#;
        let event = parse_gateway_payload(raw).unwrap();
        let GatewayEvent::Dispatch { kind: DispatchKind::MessageCreate(msg), .. } = event else {
            panic!("expected a MessageCreate dispatch");
        };
        assert!(msg.mentions.is_empty(), "mentions must default to empty when absent");
        assert!(!msg.mention_everyone, "mention_everyone must default to false when absent");
        assert!(msg.mention_roles.is_empty(), "mention_roles must default to empty when absent");
        assert_eq!(msg.message_reference, None, "message_reference must default to None when absent");
    }

    #[test]
    fn parses_a_dm_message_create_with_no_guild_or_member() {
        let raw = r#"{"op":0,"s":7,"t":"MESSAGE_CREATE","d":{
            "id":"1","channel_id":"55","content":"hi","author":{"id":"9","username":"bob"}
        }}"#;
        let event = parse_gateway_payload(raw).unwrap();
        let GatewayEvent::Dispatch { kind: DispatchKind::MessageCreate(msg), .. } = event else {
            panic!("expected a MessageCreate dispatch");
        };
        assert_eq!(msg.guild_id, None);
        assert_eq!(msg.member, None);
        assert!(!msg.author.bot, "bot must default to false when omitted");
    }

    #[test]
    fn parses_resumed_and_an_unrecognized_dispatch_type_without_erroring() {
        assert_eq!(
            parse_gateway_payload(r#"{"op":0,"s":2,"t":"RESUMED","d":{}}"#).unwrap(),
            GatewayEvent::Dispatch { seq: 2, kind: DispatchKind::Resumed }
        );
        assert_eq!(
            parse_gateway_payload(r#"{"op":0,"s":3,"t":"PRESENCE_UPDATE","d":{}}"#).unwrap(),
            GatewayEvent::Dispatch { seq: 3, kind: DispatchKind::Other },
            "an unmodeled dispatch type must still surface its seq, not error"
        );
    }

    #[test]
    fn dispatch_without_a_sequence_number_is_a_parse_error() {
        let raw = r#"{"op":0,"t":"RESUMED","d":{}}"#;
        assert!(matches!(parse_gateway_payload(raw), Err(ProtocolError::MissingSequence)));
    }

    #[test]
    fn unrecognized_opcode_is_unknown_not_an_error() {
        let raw = r#"{"op":250,"d":null}"#;
        assert_eq!(parse_gateway_payload(raw).unwrap(), GatewayEvent::Unknown);
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        assert!(parse_gateway_payload("not json").is_err());
    }

    // --- Close-code classification ---

    #[test]
    fn documented_non_resumable_codes_are_rejected() {
        for code in NON_RESUMABLE_CLOSE_CODES {
            assert!(!is_resumable_close_code(*code), "{code} must be classified non-resumable");
        }
    }

    #[test]
    fn ordinary_and_unrecognized_codes_default_to_resumable() {
        // Documented resumable codes.
        for code in [4000, 4001, 4002, 4003, 4005, 4007, 4008, 4009] {
            assert!(is_resumable_close_code(code), "{code} should be resumable");
        }
        // A plain WebSocket close with no Discord-specific meaning, and an
        // entirely unrecognized code, both default to "try resume" — see
        // the doc on `NON_RESUMABLE_CLOSE_CODES`.
        assert!(is_resumable_close_code(1000));
        assert!(is_resumable_close_code(9999));
    }

    // --- Payload builders never leak more than the caller supplied ---

    #[test]
    fn identify_payload_carries_token_and_intents() {
        let payload = identify_payload("secret-token", 37378);
        assert_eq!(payload["op"], 2);
        assert_eq!(payload["d"]["token"], "secret-token");
        assert_eq!(payload["d"]["intents"], 37378);
    }

    #[test]
    fn resume_payload_carries_session_and_seq() {
        let payload = resume_payload("secret-token", "sess-1", 99);
        assert_eq!(payload["op"], 6);
        assert_eq!(payload["d"]["session_id"], "sess-1");
        assert_eq!(payload["d"]["seq"], 99);
    }

    #[test]
    fn heartbeat_payload_carries_last_seq_or_null() {
        assert_eq!(heartbeat_payload(Some(5))["d"], 5);
        assert!(heartbeat_payload(None)["d"].is_null());
    }
}
