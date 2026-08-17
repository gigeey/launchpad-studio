//! The IMAP fetch seam [`super::EmailTransport`]'s poll loop calls each
//! cycle, and the pure parsing step that turns one raw fetched message into
//! the fields the rest of the email channel needs.
//!
//! [`MailSource`] exists so the poll loop — and its dedup/security/ingest
//! logic — is unit-testable against a scripted fake without a live IMAP
//! server. [`ImapMailSource`] is the only implementation that actually talks
//! IMAP: the `imap` crate's protocol driver is synchronous, so every call
//! into it runs inside `tokio::task::spawn_blocking` rather than blocking the
//! async runtime directly.

use async_trait::async_trait;
use imap::extensions::idle::SetReadTimeout;
use mail_parser::{Address, HeaderForm, HeaderValue, MessageParser};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MailSourceError {
    #[error("IMAP error: {0}")]
    Imap(String),
    #[error("IMAP polling task failed to run: {0}")]
    Join(String),
}

/// One fetched inbound email, already reduced to the fields the security
/// check ([`super::security::evaluate_sender`]) and the ingest formatter
/// ([`super::ingest::build_ingest_text`]) need.
///
/// The IMAP UID backs an in-session dedup safety net; durable cross-restart
/// dedup relies on the server's own `\Seen` flag instead. That flag is set
/// deliberately — through [`MailSource::mark_seen`], only once a message has
/// been accepted and handed off — rather than as a side effect of fetching
/// its body, so a message this channel declines stays `\Unseen` and is
/// re-polled instead of being silently lost from the user's inbox. Once a
/// message is marked `\Seen`, `SEARCH UNSEEN` stops returning it on the next
/// poll or after a process restart.
pub struct FetchedEmail {
    pub uid: u32,
    pub from_address: String,
    pub from_display: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: Option<String>,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub message_id: Option<String>,
    /// Every `Authentication-Results` header value found on the message, in
    /// the order they appear in the raw headers — index 0 is topmost. See
    /// [`super::security::EmailMessageMeta::authentication_results`] for why
    /// that order matters.
    pub authentication_results: Vec<String>,
    pub auto_submitted: Option<String>,
    pub precedence: Option<String>,
    pub list_unsubscribe_present: bool,
    pub x_auto_response_suppress_present: bool,
    pub body_text: String,
}

/// Fetches newly-arrived (`UNSEEN`) messages from one IMAP inbox and, once a
/// message has been accepted and handed off, marks that single message
/// `\Seen`.
#[async_trait]
pub trait MailSource: Send {
    /// Fetches every `UNSEEN` message *without* marking any of them read — the
    /// fetch uses `BODY.PEEK[]`, so deciding a message's fate later can never
    /// have already committed the read flag. Marking `\Seen` is a separate,
    /// explicit step (see [`Self::mark_seen`]).
    async fn fetch_unseen(&mut self) -> Result<Vec<FetchedEmail>, MailSourceError>;

    /// Marks the message identified by `uid` `\Seen` on the server via
    /// `UID STORE <uid> +FLAGS (\Seen)`. Called only after the message has
    /// passed the security check and been handed off downstream, so a declined
    /// or undelivered message is left `\Unseen` and re-polled rather than lost.
    async fn mark_seen(&mut self, uid: u32) -> Result<(), MailSourceError>;
}

/// Real [`MailSource`]: connects (or reuses a live connection) to one IMAP
/// account and drives it inside `spawn_blocking`.
///
/// Reconnects lazily and self-heals: any IMAP error during a poll drops the
/// held session, so the next [`Self::fetch_unseen`] call reconnects and
/// re-authenticates from scratch rather than continuing to use a connection
/// that may be wedged.
pub struct ImapMailSource {
    host: String,
    port: u16,
    address: String,
    password: String,
    session: Option<imap::Session<imap::Connection>>,
}

impl ImapMailSource {
    pub fn new(host: String, port: u16, address: String, password: String) -> Self {
        Self { host, port, address, password, session: None }
    }
}

#[async_trait]
impl MailSource for ImapMailSource {
    async fn fetch_unseen(&mut self) -> Result<Vec<FetchedEmail>, MailSourceError> {
        let host = self.host.clone();
        let port = self.port;
        let address = self.address.clone();
        let password = self.password.clone();
        let mut session = self.session.take();

        let (surviving_session, result) = tokio::task::spawn_blocking(move || {
            let mut session = match session.take() {
                Some(s) => s,
                None => match connect_and_login(&host, port, &address, &password) {
                    Ok(s) => s,
                    Err(e) => return (None, Err(e)),
                },
            };
            match fetch_unseen_blocking(&mut session) {
                Ok(emails) => (Some(session), Ok(emails)),
                // Drop the session on any mid-poll failure — the connection
                // may be half-broken, so the next call reconnects clean
                // rather than retrying on a session that's likely doomed.
                Err(e) => (None, Err(e)),
            }
        })
        .await
        .map_err(|e| MailSourceError::Join(e.to_string()))?;

        self.session = surviving_session;
        result
    }

    async fn mark_seen(&mut self, uid: u32) -> Result<(), MailSourceError> {
        let mut session = self.session.take();

        let (surviving_session, result) = tokio::task::spawn_blocking(move || {
            // `mark_seen` only runs immediately after a successful poll and
            // hand-off on this same source, so the live session that fetched
            // the message is expected here. If it's somehow gone there's
            // nothing to reconnect *for* — the STORE targets a UID that only
            // means anything on the session that fetched it — so report a soft
            // error and let the next poll re-establish and re-deliver.
            let Some(mut session) = session.take() else {
                return (
                    None,
                    Err(MailSourceError::Imap("no active IMAP session to mark message \\Seen".to_string())),
                );
            };
            match mark_seen_blocking(&mut session, uid) {
                Ok(()) => (Some(session), Ok(())),
                // Drop the session on failure, mirroring `fetch_unseen`'s
                // self-heal: the next call reconnects clean rather than
                // retrying on a connection that may be wedged.
                Err(e) => (None, Err(e)),
            }
        })
        .await
        .map_err(|e| MailSourceError::Join(e.to_string()))?;

        self.session = surviving_session;
        result
    }
}

fn connect_and_login(
    host: &str,
    port: u16,
    address: &str,
    password: &str,
) -> Result<imap::Session<imap::Connection>, MailSourceError> {
    let client = imap::ClientBuilder::new(host, port)
        .connect()
        .map_err(|e| MailSourceError::Imap(format!("connect to {host}:{port} failed: {e}")))?;

    // `imap::Session` has no public way to bound its reads, so re-wrap the
    // raw connection here, before login (nothing has been read past the
    // greeting yet, so this can't drop server data). Without a read
    // timeout, a silently-dead socket (e.g. an idle drop from Gmail, a NAT,
    // or a firewall) hangs the next poll's blocking SEARCH/FETCH forever
    // instead of erroring into the reconnect-on-next-poll self-heal path
    // above.
    let mut connection = client
        .into_inner()
        .map_err(|e| MailSourceError::Imap(format!("failed to prepare connection for read timeout: {e}")))?;
    connection
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| MailSourceError::Imap(format!("set_read_timeout failed: {e}")))?;
    let client = imap::Client::new(connection);

    let mut session = client
        .login(address, password)
        .map_err(|(e, _client)| MailSourceError::Imap(format!("login failed: {e}")))?;
    session
        .select("INBOX")
        .map_err(|e| MailSourceError::Imap(format!("SELECT INBOX failed: {e}")))?;
    Ok(session)
}

/// FETCH data item used to pull each message body. `BODY.PEEK[]` returns the
/// full raw RFC822 bytes exactly as `RFC822` or a bare `BODY[]` would, but the
/// `.PEEK` form suppresses the implicit `\Seen` flag those two set. Reading a
/// message here must not commit us to it before the allow-list / auth-results
/// security check has run — the read flag is set separately, and only after a
/// message is accepted and delivered (see [`SET_SEEN_FLAGS`]).
const PEEK_FETCH_ITEM: &str = "BODY.PEEK[]";

/// STORE flag argument that marks a message read. The leading `+` *adds*
/// `\Seen` rather than replacing the flag set. Applied via
/// [`MailSource::mark_seen`], only once a message is accepted and delivered.
const SET_SEEN_FLAGS: &str = "+FLAGS (\\Seen)";

fn fetch_unseen_blocking(
    session: &mut imap::Session<imap::Connection>,
) -> Result<Vec<FetchedEmail>, MailSourceError> {
    // Gmail doesn't reliably push new mailbox state to an already-SELECTed,
    // long-lived session on a bare SEARCH — NOOP first prompts the server to
    // report untagged updates, so UNSEEN reflects what's actually in the
    // mailbox instead of a stale snapshot from connect time.
    session.noop().map_err(|e| MailSourceError::Imap(format!("NOOP failed: {e}")))?;
    let uids = session
        .uid_search("UNSEEN")
        .map_err(|e| MailSourceError::Imap(format!("SEARCH UNSEEN failed: {e}")))?;
    tracing::info!("EmailTransport: SEARCH UNSEEN returned {} message(s)", uids.len());
    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let uid_set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    // Peek-fetch the full raw message WITHOUT setting `\Seen` (see
    // `PEEK_FETCH_ITEM`). The read flag is committed separately by
    // `mark_seen`, and only after a message is accepted and delivered — so a
    // message this channel later declines stays `\Unseen` and is re-polled
    // instead of being silently lost.
    let fetches = session
        .uid_fetch(&uid_set, PEEK_FETCH_ITEM)
        .map_err(|e| MailSourceError::Imap(format!("UID FETCH failed: {e}")))?;

    let mut out = Vec::with_capacity(fetches.len());
    for fetch in fetches.iter() {
        let (Some(uid), Some(raw)) = (fetch.uid, fetch.body()) else {
            continue;
        };
        if let Some(email) = parse_fetched_message(uid, raw) {
            out.push(email);
        }
    }
    Ok(out)
}

/// Issues `UID STORE <uid> +FLAGS (\Seen)` for a single message, marking it
/// read on the server. Kept a free function mirroring
/// [`fetch_unseen_blocking`] so [`ImapMailSource::mark_seen`] can drive it
/// inside `spawn_blocking`.
fn mark_seen_blocking(session: &mut imap::Session<imap::Connection>, uid: u32) -> Result<(), MailSourceError> {
    session
        .uid_store(uid.to_string(), SET_SEEN_FLAGS)
        .map_err(|e| MailSourceError::Imap(format!("UID STORE {SET_SEEN_FLAGS} for uid {uid} failed: {e}")))?;
    Ok(())
}

/// Parses one raw RFC822 message into a [`FetchedEmail`]. Pure and
/// IMAP-independent — `uid` is threaded through separately since it comes
/// from the IMAP fetch response, not the message itself. Returns `None` only
/// when the message doesn't even parse or has no usable `From:` address,
/// since a security decision is impossible without one.
pub fn parse_fetched_message(uid: u32, raw: &[u8]) -> Option<FetchedEmail> {
    let message = MessageParser::default().parse(raw)?;

    let from = message.from().and_then(Address::first)?;
    let from_address = from.address.as_ref()?.trim().to_string();
    let from_display = from.name.as_ref().map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

    let authentication_results = message
        .header_as("Authentication-Results", HeaderForm::Raw)
        .into_iter()
        .filter_map(|v| match v {
            HeaderValue::Text(s) => Some(s.trim().to_string()),
            _ => None,
        })
        .collect();

    let auto_submitted = message.header_raw("Auto-Submitted").map(|s| s.trim().to_string());
    let precedence = message.header_raw("Precedence").map(|s| s.trim().to_string());
    let list_unsubscribe_present = message.header_raw("List-Unsubscribe").is_some();
    let x_auto_response_suppress_present = message.header_raw("X-Auto-Response-Suppress").is_some();

    let date = message.date().and_then(|d| {
        let unix_ts: i64 = (*d).into();
        chrono::DateTime::from_timestamp(unix_ts, 0)
    });

    let body_text = message
        .body_text(0)
        .map(|c| c.into_owned())
        .or_else(|| message.body_html(0).map(|html| mail_parser::decoders::html::html_to_text(&html)))
        .unwrap_or_default();

    Some(FetchedEmail {
        uid,
        from_address,
        from_display,
        to: collect_addresses(message.to()),
        cc: collect_addresses(message.cc()),
        subject: message.subject().map(str::to_string),
        date,
        message_id: message.message_id().map(str::to_string),
        authentication_results,
        auto_submitted,
        precedence,
        list_unsubscribe_present,
        x_auto_response_suppress_present,
        body_text,
    })
}

fn collect_addresses(addr: Option<&Address>) -> Vec<String> {
    let Some(addr) = addr else {
        return Vec::new();
    };
    match addr {
        Address::List(list) => list.iter().filter_map(|a| a.address.as_deref().map(str::to_string)).collect(),
        Address::Group(groups) => groups
            .iter()
            .flat_map(|g| g.addresses.iter())
            .filter_map(|a| a.address.as_deref().map(str::to_string))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_text_message_with_authentication_results() {
        let raw = b"From: \"Sender Name\" <sender@example.com>\r\n\
To: agent@example.org\r\n\
Cc: watcher@example.org\r\n\
Subject: Hello there\r\n\
Date: Mon, 1 Jan 2024 12:00:00 +0000\r\n\
Message-ID: <abc123@example.com>\r\n\
Authentication-Results: mx.example.org; dmarc=pass\r\n\
Content-Type: text/plain\r\n\
\r\n\
Hello, this is the body.\r\n";

        let email = parse_fetched_message(42, raw).expect("message parses");
        assert_eq!(email.uid, 42);
        assert_eq!(email.from_address, "sender@example.com");
        assert_eq!(email.from_display.as_deref(), Some("Sender Name"));
        assert_eq!(email.to, vec!["agent@example.org".to_string()]);
        assert_eq!(email.cc, vec!["watcher@example.org".to_string()]);
        assert_eq!(email.subject.as_deref(), Some("Hello there"));
        assert_eq!(email.message_id.as_deref(), Some("abc123@example.com"));
        assert_eq!(email.authentication_results, vec!["mx.example.org; dmarc=pass".to_string()]);
        assert!(email.date.is_some());
        assert!(email.body_text.contains("Hello, this is the body."));
    }

    #[test]
    fn collects_multiple_authentication_results_headers_in_order() {
        let raw = b"From: sender@example.com\r\n\
To: agent@example.org\r\n\
Authentication-Results: mx.example.org; dmarc=pass\r\n\
Authentication-Results: forged.invalid; dmarc=fail\r\n\
Subject: test\r\n\
\r\n\
body\r\n";

        let email = parse_fetched_message(1, raw).expect("message parses");
        assert_eq!(
            email.authentication_results,
            vec!["mx.example.org; dmarc=pass".to_string(), "forged.invalid; dmarc=fail".to_string()],
            "must preserve header order, topmost first"
        );
    }

    #[test]
    fn falls_back_to_html_body_when_no_plain_text_part_exists() {
        let raw = b"From: sender@example.com\r\n\
To: agent@example.org\r\n\
Subject: html only\r\n\
Content-Type: text/html\r\n\
\r\n\
<p>Hello <b>world</b></p>\r\n";

        let email = parse_fetched_message(2, raw).expect("message parses");
        assert!(email.body_text.contains("Hello"));
        assert!(email.body_text.contains("world"));
        assert!(!email.body_text.contains('<'), "html tags should be stripped");
    }

    #[test]
    fn detects_bulk_and_missing_headers() {
        let raw = b"From: sender@example.com\r\n\
To: agent@example.org\r\n\
Subject: bulk\r\n\
Precedence: bulk\r\n\
List-Unsubscribe: <mailto:unsub@example.com>\r\n\
\r\n\
body\r\n";

        let email = parse_fetched_message(3, raw).expect("message parses");
        assert_eq!(email.precedence.as_deref(), Some("bulk"));
        assert!(email.list_unsubscribe_present);
        assert!(!email.x_auto_response_suppress_present);
        assert!(email.authentication_results.is_empty());
    }

    #[test]
    fn message_with_no_from_header_fails_to_parse_into_a_fetched_email() {
        let raw = b"To: agent@example.org\r\nSubject: no sender\r\n\r\nbody\r\n";
        assert!(parse_fetched_message(4, raw).is_none());
    }

    #[test]
    fn body_fetch_item_is_the_peek_variant_that_does_not_set_seen() {
        // The whole fix hinges on this constant: a bare `RFC822` or `BODY[]`
        // fetch implicitly marks every fetched message `\Seen` server-side
        // BEFORE the security check runs, so a declined message would be lost.
        // `BODY.PEEK[]` returns the same bytes without that side effect.
        assert!(PEEK_FETCH_ITEM.contains("PEEK"), "fetch must use the .PEEK variant");
        assert!(
            !PEEK_FETCH_ITEM.eq_ignore_ascii_case("RFC822"),
            "must not revert to RFC822, which implicitly sets \\Seen"
        );
        assert!(
            !PEEK_FETCH_ITEM.eq_ignore_ascii_case("BODY[]"),
            "must not revert to a bare BODY[], which also implicitly sets \\Seen"
        );
    }

    #[test]
    fn set_seen_flags_adds_the_seen_flag() {
        assert!(SET_SEEN_FLAGS.contains("\\Seen"), "must set the \\Seen flag");
        assert!(
            SET_SEEN_FLAGS.trim_start().starts_with('+'),
            "leading + must ADD \\Seen rather than replacing the message's flag set"
        );
    }
}
