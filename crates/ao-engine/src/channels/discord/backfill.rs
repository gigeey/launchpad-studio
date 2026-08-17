//! Transport-layer conversation-history assembly for Discord, run **before**
//! an inbound message reaches [`crate::channels::submit_inbound_message`] —
//! never an agent tool, never anything model-callable. With mention-gating
//! in place ([`super::engagement`]), the bot stops reading most messages in
//! a shared channel, so the moment it *is* pulled into a conversation it has
//! no memory of what preceded the message that triggered it. This module
//! fills that gap deterministically, on the caller's behalf, so the model
//! is simply handed context it never had to ask for.
//!
//! Two independent fetch modes, chosen by the caller (`super::runner`) based
//! on facts this module doesn't itself decide:
//!
//! - [`fetch_thread_backfill`] — a flat window of the messages immediately
//!   preceding the trigger, fetched once per conversation on the COLD->WARM
//!   transition (never per message). Meant for threads, where every message
//!   in the window genuinely belongs to the one conversation the thread is.
//! - [`fetch_reply_chain_backfill`] — walks a `message_reference` chain
//!   upward from the triggering message, one hop at a time, up to
//!   [`MAX_REPLY_CHAIN_DEPTH`] links. Meant for a mention landing in an
//!   ordinary (non-thread) guild channel, where a time-windowed fetch would
//!   pull in unrelated, multi-topic chatter — the reply chain is the only
//!   part of that channel's history actually relevant to the message that
//!   triggered the bot.
//!
//! [`BackfillSeam`] isolates the two REST calls both modes are built from —
//! `GET /channels/{channel_id}/messages` and
//! `GET /channels/{channel_id}/messages/{message_id}` — the same
//! `Authorization: Bot {token}` REST boundary [`super::outbound_seam`] and
//! [`super::channel_meta`] already isolate their own calls behind, so both
//! fetch modes are provable against a scripted fake rather than a live
//! network call. [`ReqwestBackfillSeam`] is the only implementation that
//! actually calls Discord.
//!
//! Every failure path — a network error, a non-2xx status, an unparseable
//! body, a chain link with no further reference — logs a warning and
//! resolves to an empty result rather than propagating: a history fetch is
//! enrichment, never a precondition, so it must never block or fail the
//! inbound message it's decorating. A 403 specifically means the bot lacks
//! Discord's `READ_MESSAGE_HISTORY` permission in the channel, which is
//! common enough as a real-world misconfiguration that it gets its own
//! actionable warning naming the permission, rather than folding into the
//! generic non-success case.

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use tracing::warn;

use super::protocol::MessageReference;

/// Maximum number of `message_reference` hops [`fetch_reply_chain_backfill`]
/// follows upward from the triggering message. A reply chain is usually
/// short (a handful of back-and-forth messages); capping it bounds both the
/// REST call count and how much unrelated-by-now context gets pulled into a
/// single turn if a chain happens to run deep.
pub const MAX_REPLY_CHAIN_DEPTH: u32 = 5;

/// One prior message resolved by either fetch mode, reduced to exactly what
/// [`format_backfill`] needs: a human-readable author label and the message
/// text. Deliberately doesn't carry the message id, channel, or any other
/// wire detail — nothing downstream of this module needs them, and keeping
/// this type minimal keeps `format_backfill` simple to keep exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfilledMessage {
    pub author: String,
    pub content: String,
}

impl From<RawMessage> for BackfilledMessage {
    fn from(raw: RawMessage) -> Self {
        Self { author: raw.author.label(), content: raw.content }
    }
}

/// The subset of Discord's message object both REST calls in this module
/// return, independent of [`super::protocol::MessageCreateEvent`] (the
/// Gateway `MESSAGE_CREATE` shape) even though the two overlap — this one
/// exists purely to walk a reply chain and label authors, so it carries
/// nothing `MessageCreateEvent` needs for gateway dispatch and vice versa.
#[derive(Debug, Clone, Deserialize)]
struct RawMessage {
    #[serde(default)]
    content: String,
    author: RawAuthor,
    /// Present when this message is itself a reply — [`fetch_reply_chain_backfill`]
    /// follows this one hop further up the chain.
    #[serde(default)]
    message_reference: Option<MessageReference>,
}

/// The subset of Discord's user object needed to label a backfilled
/// message. `global_name` is the account's server-independent display
/// name (nullable — not every account has set one); when present it's
/// what a human actually recognizes the author as, so it takes priority
/// over `username`. The raw snowflake `id` is never surfaced here at
/// all — unreadable to a model, and no caller of this module needs it.
#[derive(Debug, Clone, Deserialize)]
struct RawAuthor {
    username: String,
    #[serde(default)]
    global_name: Option<String>,
}

impl RawAuthor {
    /// Never returns an empty string: an explicitly-blank `global_name`
    /// (as opposed to an absent one) still falls back to `username`, since
    /// Discord usernames are themselves never empty.
    fn label(&self) -> String {
        match &self.global_name {
            Some(name) if !name.is_empty() => name.clone(),
            _ => self.username.clone(),
        }
    }
}

#[derive(Debug, Clone, Error)]
enum BackfillSeamError {
    #[error("discord history fetch failed: {0}")]
    Request(String),
    /// A 403 on either endpoint this module calls means one thing in
    /// practice: the bot's role lacks `READ_MESSAGE_HISTORY` in this
    /// channel. Kept as its own variant (rather than folding into `Status`)
    /// so the caller can log the actionable, permission-naming warning
    /// instead of a generic status-code message.
    #[error("discord history fetch was forbidden (403)")]
    Forbidden,
    #[error("discord history fetch returned a non-success status: {status}")]
    Status { status: u16 },
    #[error("discord history fetch response did not parse: {0}")]
    Parse(String),
}

/// The network boundary both fetch modes' REST calls go through — the
/// backfill analogue of [`super::outbound_seam::DiscordSendSeam`] and
/// [`super::channel_meta::ChannelMetaSeam`]. [`ReqwestBackfillSeam`] is the
/// only implementation that actually calls the Discord REST API; this
/// module's tests drive both fetch modes against an in-memory fake instead.
#[async_trait]
trait BackfillSeam: Send + Sync {
    /// `GET /channels/{channel_id}/messages?limit={limit}&before={before_message_id}`.
    async fn fetch_before(
        &self,
        token: &str,
        channel_id: &str,
        before_message_id: &str,
        limit: u32,
    ) -> Result<Vec<RawMessage>, BackfillSeamError>;

    /// `GET /channels/{channel_id}/messages/{message_id}`.
    async fn fetch_message(
        &self,
        token: &str,
        channel_id: &str,
        message_id: &str,
    ) -> Result<RawMessage, BackfillSeamError>;
}

/// Real [`BackfillSeam`]: fetches straight from the Discord REST API,
/// authenticated the same way every other REST call in this transport is —
/// an `Authorization: Bot <token>` header, never the token in a log line.
struct ReqwestBackfillSeam {
    http: reqwest::Client,
}

/// Maps a completed response's status to a [`BackfillSeamError`] without
/// consuming it, so the caller can still read the body afterward on
/// success — mirrors the status-then-body sequencing
/// [`super::channel_meta::resolve_channel_meta`] already uses.
fn check_status(response: &reqwest::Response) -> Result<(), BackfillSeamError> {
    if response.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(BackfillSeamError::Forbidden);
    }
    if !response.status().is_success() {
        return Err(BackfillSeamError::Status { status: response.status().as_u16() });
    }
    Ok(())
}

#[async_trait]
impl BackfillSeam for ReqwestBackfillSeam {
    async fn fetch_before(
        &self,
        token: &str,
        channel_id: &str,
        before_message_id: &str,
        limit: u32,
    ) -> Result<Vec<RawMessage>, BackfillSeamError> {
        let url = format!(
            "https://discord.com/api/v10/channels/{channel_id}/messages?limit={limit}&before={before_message_id}"
        );
        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bot {token}"))
            .send()
            .await
            .map_err(|e| BackfillSeamError::Request(e.to_string()))?;
        check_status(&response)?;
        response.json::<Vec<RawMessage>>().await.map_err(|e| BackfillSeamError::Parse(e.to_string()))
    }

    async fn fetch_message(
        &self,
        token: &str,
        channel_id: &str,
        message_id: &str,
    ) -> Result<RawMessage, BackfillSeamError> {
        let url = format!("https://discord.com/api/v10/channels/{channel_id}/messages/{message_id}");
        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bot {token}"))
            .send()
            .await
            .map_err(|e| BackfillSeamError::Request(e.to_string()))?;
        check_status(&response)?;
        response.json::<RawMessage>().await.map_err(|e| BackfillSeamError::Parse(e.to_string()))
    }
}

/// Logs `e` as a warning against `channel_id`, giving the 403 case its own
/// actionable, permission-naming message rather than the generic one —
/// shared by both fetch modes' error paths.
fn log_backfill_failure(what: &str, channel_id: &str, e: &BackfillSeamError) {
    if matches!(e, BackfillSeamError::Forbidden) {
        warn!(
            channel_id = %channel_id,
            "DiscordTransport: {what} got a 403 from Discord — the bot is missing the READ_MESSAGE_HISTORY permission in this channel"
        );
    } else {
        warn!(channel_id = %channel_id, "DiscordTransport: {what} failed: {e}");
    }
}

/// THREAD BACKFILL: fetches up to `limit` messages immediately preceding
/// `before_message_id` on `channel_id`, in chronological (oldest-first)
/// order. `limit` is `super::runner`'s pass-through of the binding's own
/// `ChannelKindConfig::Discord::backfill_limit` (default 20 — Discord's own
/// `messages` endpoint caps this at 100); `runner` never calls this at all
/// when that config is `0`. Meant to fire exactly once per conversation, on
/// the COLD->WARM transition — never per message; enforcing that cadence is
/// the caller's job (`super::engagement` decides COLD/WARM, `super::runner`
/// calls this only on the transition), not this function's.
///
/// Any failure — network error, non-2xx, unparseable body — logs a warning
/// and resolves to an empty `Vec`, never propagating: a failed history
/// fetch must never block or fail the inbound message it's meant to
/// decorate. Discord returns newest-first; this reverses it before
/// returning, since every other consumer of a `Vec<BackfilledMessage>`
/// (starting with [`format_backfill`]) expects chronological order.
pub async fn fetch_thread_backfill(
    http: &reqwest::Client,
    token: &str,
    channel_id: &str,
    before_message_id: &str,
    limit: u32,
) -> Vec<BackfilledMessage> {
    let seam = ReqwestBackfillSeam { http: http.clone() };
    fetch_thread_backfill_via_seam(&seam, token, channel_id, before_message_id, limit).await
}

async fn fetch_thread_backfill_via_seam(
    seam: &dyn BackfillSeam,
    token: &str,
    channel_id: &str,
    before_message_id: &str,
    limit: u32,
) -> Vec<BackfilledMessage> {
    match seam.fetch_before(token, channel_id, before_message_id, limit).await {
        Ok(mut messages) => {
            messages.reverse();
            messages.into_iter().map(BackfilledMessage::from).collect()
        }
        Err(e) => {
            log_backfill_failure("thread history backfill", channel_id, &e);
            Vec::new()
        }
    }
}

/// REPLY-CHAIN BACKFILL: starting from `starting_reference` (the triggering
/// message's own `message_reference`), fetches the message it points at,
/// then follows *that* message's own `message_reference` one hop further,
/// and so on up to [`MAX_REPLY_CHAIN_DEPTH`] hops total. `channel_id` is the
/// fallback used only when a hop's reference omits its own `channel_id`
/// (replies are almost always same-channel, but the field is optional on
/// the wire).
///
/// Stops early — cleanly, without erroring — the moment any hop fails
/// (network error, non-2xx, unparseable body) or carries no further
/// `message_reference`; whatever was collected before that point is still
/// returned. This is deliberately surgical: unlike [`fetch_thread_backfill`],
/// it never widens into a time window of channel history, since a shared
/// guild channel's main history is noisy and multi-topic in a way a reply
/// chain specifically is not.
///
/// Returned oldest-first (the root of the chain first), matching
/// [`fetch_thread_backfill`]'s ordering contract and [`format_backfill`]'s
/// expectation.
pub async fn fetch_reply_chain_backfill(
    http: &reqwest::Client,
    token: &str,
    channel_id: &str,
    starting_reference: &MessageReference,
) -> Vec<BackfilledMessage> {
    let seam = ReqwestBackfillSeam { http: http.clone() };
    fetch_reply_chain_backfill_via_seam(&seam, token, channel_id, starting_reference).await
}

async fn fetch_reply_chain_backfill_via_seam(
    seam: &dyn BackfillSeam,
    token: &str,
    channel_id: &str,
    starting_reference: &MessageReference,
) -> Vec<BackfilledMessage> {
    let mut collected = Vec::new();
    let mut next_reference = Some(starting_reference.clone());
    let mut hops = 0u32;

    while hops < MAX_REPLY_CHAIN_DEPTH {
        let Some(reference) = next_reference.take() else {
            break;
        };
        let Some(message_id) = reference.message_id.as_deref() else {
            break;
        };
        let hop_channel_id = reference.channel_id.as_deref().unwrap_or(channel_id);

        match seam.fetch_message(token, hop_channel_id, message_id).await {
            Ok(raw) => {
                next_reference = raw.message_reference.clone();
                collected.push(BackfilledMessage::from(raw));
                hops += 1;
            }
            Err(e) => {
                log_backfill_failure("reply-chain backfill", hop_channel_id, &e);
                break;
            }
        }
    }

    // Collected walking upward from the trigger (nearest link first, root
    // last); reverse so the root comes first, matching chronological order.
    collected.reverse();
    collected
}

const BACKFILL_HEADER: &str = "[Earlier messages in this Discord conversation, oldest first]";
const BACKFILL_FOOTER: &str = "[End of earlier messages]";

/// Renders `messages` (already in chronological order — both fetch modes in
/// this module guarantee that) into a single clearly-delimited,
/// author-labeled block meant to be injected ahead of the triggering
/// message in the agent's turn. Every author is labeled by
/// [`RawAuthor::label`]'s username-or-global-name rule, including the bot's
/// own past messages and other bots' messages — neither is filtered out,
/// since both are legitimate conversational context, just labeled like any
/// other participant. Content is passed through verbatim: nothing is
/// escaped or normalized.
///
/// An empty `messages` slice — including "the fetch failed" and "the fetch
/// succeeded but found nothing," both of which the fetch functions above
/// already reduce to an empty `Vec` — returns an empty `String`. The caller
/// must treat that as "inject nothing," never as an empty-but-present
/// backfill block.
pub fn format_backfill(messages: &[BackfilledMessage]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(messages.len() + 2);
    lines.push(BACKFILL_HEADER.to_string());
    for message in messages {
        lines.push(format!("{}: {}", message.author, message.content));
    }
    lines.push(BACKFILL_FOOTER.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    use super::*;

    fn raw(username: &str, global_name: Option<&str>, content: &str, reference: Option<MessageReference>) -> RawMessage {
        RawMessage {
            content: content.to_string(),
            author: RawAuthor { username: username.to_string(), global_name: global_name.map(str::to_string) },
            message_reference: reference,
        }
    }

    fn reference_to(message_id: &str) -> MessageReference {
        MessageReference { message_id: Some(message_id.to_string()), channel_id: None, guild_id: None }
    }

    // --- RawAuthor::label ---

    #[test]
    fn label_prefers_global_name_over_username() {
        let author = RawAuthor { username: "alice123".to_string(), global_name: Some("Alice".to_string()) };
        assert_eq!(author.label(), "Alice");
    }

    #[test]
    fn label_falls_back_to_username_when_global_name_absent() {
        let author = RawAuthor { username: "alice123".to_string(), global_name: None };
        assert_eq!(author.label(), "alice123");
    }

    #[test]
    fn label_falls_back_to_username_when_global_name_is_blank() {
        let author = RawAuthor { username: "alice123".to_string(), global_name: Some(String::new()) };
        assert_eq!(author.label(), "alice123");
    }

    // --- format_backfill ---

    #[test]
    fn format_backfill_matches_the_exact_expected_block() {
        let messages = vec![
            BackfilledMessage { author: "alice".to_string(), content: "hey can someone look at the deploy".to_string() },
            BackfilledMessage { author: "bob".to_string(), content: "I think it's the migration".to_string() },
        ];
        let expected = "[Earlier messages in this Discord conversation, oldest first]\n\
                         alice: hey can someone look at the deploy\n\
                         bob: I think it's the migration\n\
                         [End of earlier messages]";
        assert_eq!(format_backfill(&messages), expected);
    }

    #[test]
    fn format_backfill_of_an_empty_slice_is_an_empty_string() {
        assert_eq!(format_backfill(&[]), "");
    }

    #[test]
    fn format_backfill_labels_bot_authors_like_any_other_participant() {
        let messages = vec![
            BackfilledMessage { author: "helper-bot".to_string(), content: "on it".to_string() },
            BackfilledMessage { author: "another-bot".to_string(), content: "same here".to_string() },
        ];
        let out = format_backfill(&messages);
        assert!(out.contains("helper-bot: on it"), "a bot author must appear in the block, not be filtered out");
        assert!(out.contains("another-bot: same here"), "another bot's message must appear too");
    }

    // --- Fake seam ---

    /// Records every call it receives and returns pre-scripted results, so
    /// both fetch modes are provable without a live network call. `before`
    /// scripts [`BackfillSeam::fetch_before`]'s single response; `messages`
    /// scripts [`BackfillSeam::fetch_message`] per message id, so a reply
    /// chain of arbitrary shape (including a mid-chain failure) can be built.
    #[derive(Default)]
    struct FakeBackfillSeam {
        before_calls: StdMutex<Vec<(String, String, u32)>>,
        before_result: StdMutex<Option<Result<Vec<RawMessage>, BackfillSeamError>>>,
        message_calls: StdMutex<Vec<(String, String)>>,
        messages: HashMap<String, Result<RawMessage, BackfillSeamError>>,
    }

    impl FakeBackfillSeam {
        fn with_before_result(result: Result<Vec<RawMessage>, BackfillSeamError>) -> Self {
            Self { before_result: StdMutex::new(Some(result)), ..Default::default() }
        }

        fn with_messages(messages: HashMap<String, Result<RawMessage, BackfillSeamError>>) -> Self {
            Self { messages, ..Default::default() }
        }

        fn before_calls(&self) -> Vec<(String, String, u32)> {
            self.before_calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn message_call_ids(&self) -> Vec<String> {
            self.message_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(_, message_id)| message_id.clone())
                .collect()
        }
    }

    #[async_trait]
    impl BackfillSeam for FakeBackfillSeam {
        async fn fetch_before(
            &self,
            _token: &str,
            channel_id: &str,
            before_message_id: &str,
            limit: u32,
        ) -> Result<Vec<RawMessage>, BackfillSeamError> {
            self.before_calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                channel_id.to_string(),
                before_message_id.to_string(),
                limit,
            ));
            self.before_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .expect("fetch_before called more than once on a single-shot fake")
        }

        async fn fetch_message(
            &self,
            _token: &str,
            channel_id: &str,
            message_id: &str,
        ) -> Result<RawMessage, BackfillSeamError> {
            self.message_calls.lock().unwrap_or_else(|e| e.into_inner()).push((
                channel_id.to_string(),
                message_id.to_string(),
            ));
            self.messages
                .get(message_id)
                .cloned()
                .unwrap_or_else(|| Err(BackfillSeamError::Request(format!("fake has no script for message {message_id}"))))
        }
    }

    // --- fetch_thread_backfill_via_seam ---

    #[tokio::test]
    async fn thread_backfill_reverses_newest_first_response_to_chronological() {
        let seam = FakeBackfillSeam::with_before_result(Ok(vec![
            raw("bob", None, "newest", None),
            raw("alice", None, "middle", None),
            raw("alice", None, "oldest", None),
        ]));

        let result = fetch_thread_backfill_via_seam(&seam, "token", "chan-1", "trigger-msg", 20).await;

        assert_eq!(
            result,
            vec![
                BackfilledMessage { author: "alice".to_string(), content: "oldest".to_string() },
                BackfilledMessage { author: "alice".to_string(), content: "middle".to_string() },
                BackfilledMessage { author: "bob".to_string(), content: "newest".to_string() },
            ]
        );
    }

    #[tokio::test]
    async fn thread_backfill_passes_the_requested_limit_and_before_id_through() {
        let seam = FakeBackfillSeam::with_before_result(Ok(vec![]));

        let _ = fetch_thread_backfill_via_seam(&seam, "token", "chan-1", "trigger-msg", 7).await;

        assert_eq!(seam.before_calls(), vec![("chan-1".to_string(), "trigger-msg".to_string(), 7)]);
    }

    #[tokio::test]
    async fn thread_backfill_of_an_empty_response_yields_an_empty_result() {
        let seam = FakeBackfillSeam::with_before_result(Ok(vec![]));

        let result = fetch_thread_backfill_via_seam(&seam, "token", "chan-1", "trigger-msg", 20).await;

        assert!(result.is_empty());
        assert_eq!(format_backfill(&result), "", "an empty fetch must format to an empty string");
    }

    #[tokio::test]
    async fn thread_backfill_403_yields_empty_result_and_logs_the_permission_specific_warning() {
        use std::io;
        use std::sync::{Arc as StdArc, Mutex as StdMutex2};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct SharedBufWriter(StdArc<StdMutex2<Vec<u8>>>);
        impl io::Write for SharedBufWriter {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for SharedBufWriter {
            type Writer = SharedBufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = StdArc::new(StdMutex2::new(Vec::<u8>::new()));
        let writer = SharedBufWriter(StdArc::clone(&buf));
        let subscriber =
            tracing_subscriber::fmt().with_writer(writer).with_ansi(false).with_max_level(tracing::Level::WARN).finish();
        // `#[tokio::test]` defaults to a current-thread runtime, so this
        // thread-local guard stays in scope across the `.await` below —
        // no need for a synchronous `with_default` + separate executor.
        let _guard = tracing::subscriber::set_default(subscriber);

        let seam = FakeBackfillSeam::with_before_result(Err(BackfillSeamError::Forbidden));
        let result = fetch_thread_backfill_via_seam(&seam, "token", "chan-1", "trigger-msg", 20).await;

        assert!(result.is_empty(), "a 403 must resolve to an empty backfill, never propagate");

        let captured = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8");
        assert!(
            captured.contains("READ_MESSAGE_HISTORY"),
            "a 403 must log a warning naming the missing READ_MESSAGE_HISTORY permission:\n{captured}"
        );
    }

    // --- fetch_reply_chain_backfill_via_seam ---

    #[tokio::test]
    async fn reply_chain_follows_three_links_to_the_root() {
        let mut messages = HashMap::new();
        messages.insert("10".to_string(), Ok(raw("alice", None, "c10", Some(reference_to("9")))));
        messages.insert("9".to_string(), Ok(raw("bob", None, "c9", Some(reference_to("8")))));
        messages.insert("8".to_string(), Ok(raw("alice", None, "c8", None)));
        let seam = FakeBackfillSeam::with_messages(messages);

        let result = fetch_reply_chain_backfill_via_seam(&seam, "token", "chan-1", &reference_to("10")).await;

        assert_eq!(
            result,
            vec![
                BackfilledMessage { author: "alice".to_string(), content: "c8".to_string() },
                BackfilledMessage { author: "bob".to_string(), content: "c9".to_string() },
                BackfilledMessage { author: "alice".to_string(), content: "c10".to_string() },
            ]
        );
    }

    #[tokio::test]
    async fn reply_chain_stops_at_max_depth_even_when_a_further_link_exists() {
        // A chain of 8 links (ids "8".."1"), each referencing the next lower
        // id; every link (including the deepest one reached) still carries
        // a further reference, so only MAX_REPLY_CHAIN_DEPTH stops it.
        let mut messages = HashMap::new();
        for id in 1..=8u32 {
            let content = format!("c{id}");
            let reference = if id > 1 { Some(reference_to(&(id - 1).to_string())) } else { Some(reference_to("0")) };
            messages.insert(id.to_string(), Ok(raw("alice", None, &content, reference)));
        }
        let seam = FakeBackfillSeam::with_messages(messages);

        let result = fetch_reply_chain_backfill_via_seam(&seam, "token", "chan-1", &reference_to("8")).await;

        assert_eq!(result.len(), MAX_REPLY_CHAIN_DEPTH as usize, "must stop at exactly MAX_REPLY_CHAIN_DEPTH links");
        assert_eq!(
            result.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["c4", "c5", "c6", "c7", "c8"],
            "must collect the 5 links nearest the trigger, oldest first"
        );
        assert!(
            !seam.message_call_ids().contains(&"3".to_string()),
            "must never fetch past the depth cap even though message 4 still referenced message 3"
        );
    }

    #[tokio::test]
    async fn reply_chain_stops_cleanly_when_a_link_has_no_further_reference() {
        let mut messages = HashMap::new();
        messages.insert("20".to_string(), Ok(raw("alice", None, "c20", Some(reference_to("21")))));
        messages.insert("21".to_string(), Ok(raw("bob", None, "c21", None)));
        let seam = FakeBackfillSeam::with_messages(messages);

        let result = fetch_reply_chain_backfill_via_seam(&seam, "token", "chan-1", &reference_to("20")).await;

        assert_eq!(
            result,
            vec![
                BackfilledMessage { author: "bob".to_string(), content: "c21".to_string() },
                BackfilledMessage { author: "alice".to_string(), content: "c20".to_string() },
            ]
        );
    }

    #[tokio::test]
    async fn reply_chain_stops_cleanly_when_a_link_errors() {
        let mut messages = HashMap::new();
        messages.insert("30".to_string(), Ok(raw("alice", None, "c30", Some(reference_to("31")))));
        messages.insert("31".to_string(), Err(BackfillSeamError::Status { status: 500 }));
        let seam = FakeBackfillSeam::with_messages(messages);

        let result = fetch_reply_chain_backfill_via_seam(&seam, "token", "chan-1", &reference_to("30")).await;

        assert_eq!(result, vec![BackfilledMessage { author: "alice".to_string(), content: "c30".to_string() }]);
    }

    #[tokio::test]
    async fn reply_chain_immediately_missing_reference_yields_an_empty_result() {
        let seam = FakeBackfillSeam::with_messages(HashMap::new());
        let empty_reference = MessageReference { message_id: None, channel_id: None, guild_id: None };

        let result = fetch_reply_chain_backfill_via_seam(&seam, "token", "chan-1", &empty_reference).await;

        assert!(result.is_empty());
        assert!(seam.message_call_ids().is_empty(), "a reference with no message_id must never call the seam");
    }
}
