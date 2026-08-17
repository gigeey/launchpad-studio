//! The Socket Mode network I/O boundary the Slack runner drives its
//! connect/dispatch/reconnect loop against — the Slack analogue of
//! [`crate::channels::discord::gateway_seam::GatewaySeam`]. [`SlackSocketSeam`]
//! exists so that state machine is unit-testable against a scripted fake
//! ([`FakeSlackSocketSeam`]) without a live Slack workspace.
//! [`TungsteniteSlackSocketSeam`] is the only implementation that actually
//! opens a connection.
//!
//! This seam owns `apps.connections.open` itself and returns the `wss://`
//! URL it hands back — **not** [`super::web_api_seam::SlackApiSeam::connections_open`],
//! which is a setup-time check that deliberately discards the URL rather
//! than open a live socket. That call exists to answer "does this app-level
//! token work," one-shot, from the setup screen; this one exists to actually
//! obtain the endpoint the runner connects to, and is called with the same
//! app-level token (`xapp-...`, scope `connections:write`) every time a
//! connection (including a reconnect) is opened, since Slack mints a fresh,
//! single-use URL on every call.
//!
//! [`SlackSocketSeam`] is deliberately **not** shaped like `GatewaySeam`:
//! there is no `arm_heartbeat`/`next_event(last_seq)` pair, because Socket
//! Mode has no client-driven heartbeat and no sequence number to carry —
//! acks are per-envelope, keyed on `envelope_id`, which is a `runner`/
//! `protocol` concern layered on top of this seam, not this seam's job.
//! [`recv`](SlackSocketSeam::recv) hands back a raw [`SocketFrame`],
//! unparsed; envelope parsing is `protocol.rs`'s job once it exists.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const SLACK_API_BASE: &str = "https://slack.com/api";

/// Bound on the one-shot `apps.connections.open` call. Mirrors
/// `web_api_seam`'s `HTTP_REQUEST_TIMEOUT` — generous relative to Slack's
/// documented response times, firm enough not to stall a reconnect attempt
/// indefinitely.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a single `next()` poll on the live socket is allowed to sit with
/// nothing arriving — not even one of Slack's own periodic pings, which this
/// seam answers internally without ever surfacing to the caller — before
/// [`TungsteniteSlackSocketSeam::recv`] gives up and reports the connection
/// as dead.
///
/// Unlike Discord's Gateway, Socket Mode has no client-driven heartbeat
/// (`runner.rs`'s module doc), so there is nothing else in this transport
/// that would ever notice a connection the network has silently dropped —
/// no TCP reset, no close frame, no `disconnect` envelope, just nothing,
/// ever again. Slack's server pings roughly every 30 seconds to keep an idle
/// connection alive; this is a comfortable multiple of that so a merely
/// quiet channel is never mistaken for a dead one, while a connection stuck
/// silent for much longer than Slack's own keep-alive cadence is still
/// caught and handed back to the runner's existing (already-tested)
/// read-error → reconnect path, rather than hanging forever with no log line
/// at all.
const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(75);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlackSocketSeamError {
    /// `apps.connections.open` itself failed — a network error, a non-2xx
    /// status, an `{"ok": false, ...}` body, or a response with no `url`
    /// field. Distinguishing *why* (network vs. auth) is
    /// `SlackApiCallError`'s job for the setup-time check; this variant is
    /// coarser because a runner reconnect loop reacts to it the same way
    /// regardless (back off, retry) rather than reporting it per-check.
    #[error("apps.connections.open failed: {0}")]
    ConnectionsOpen(String),
    #[error("socket connect failed: {0}")]
    Connect(String),
    #[error("socket is not connected")]
    NotConnected,
    #[error("socket read failed: {0}")]
    Read(String),
    #[error("socket write failed: {0}")]
    Write(String),
    #[error("socket connection was closed by the peer (code {code:?})")]
    ClosedByPeer { code: Option<u16> },
    #[error("socket connection ended without a close frame")]
    StreamEnded,
    /// No frame — not even a ping — arrived within [`IDLE_READ_TIMEOUT`].
    /// Treated identically to any other read failure by the runner's
    /// reconnect logic; see that constant's doc for why this exists.
    #[error("no frame received within the idle window, treating the connection as dead")]
    IdleTimeout,
}

/// One inbound frame off the Socket Mode connection, before any Slack
/// envelope parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketFrame {
    /// A text frame — in practice always one JSON-encoded Socket Mode
    /// envelope (`hello`, `events_api`, `disconnect`, `slash_commands`,
    /// `interactive`, ...). Left as raw text here; decoding which of those
    /// it is belongs to `protocol.rs`.
    Text(String),
}

/// The network I/O boundary for one Socket Mode connection's lifetime —
/// opened by [`Self::connect`] (which itself performs `apps.connections.open`
/// to obtain the URL it connects to), read via [`Self::recv`], written via
/// [`Self::send`], and always ended via [`Self::close`] before a caller opens
/// the next one.
#[async_trait]
pub trait SlackSocketSeam: Send {
    /// Calls `apps.connections.open` with `app_token` as a bearer token to
    /// obtain a fresh `wss://` URL, then opens a live WebSocket connection to
    /// it. Returns the URL on success. Callers reconnecting (e.g. after a
    /// `disconnect` envelope) must call this again rather than reusing a
    /// previously returned URL — each call mints a new, single-use one.
    async fn connect(&mut self, app_token: &str) -> Result<String, SlackSocketSeamError>;

    /// Sends one text frame — in practice a JSON-encoded envelope ack
    /// (`{"envelope_id": ...}`) or a reply payload. This seam has no
    /// knowledge of Slack's envelope shape; it only writes text.
    async fn send(&mut self, text: &str) -> Result<(), SlackSocketSeamError>;

    /// Waits for the next inbound frame.
    async fn recv(&mut self) -> Result<SocketFrame, SlackSocketSeamError>;

    /// Explicitly closes the socket. Idempotent — closing an already-closed
    /// or never-opened seam is a no-op, not an error, so callers can call it
    /// unconditionally on every reconnect path (including Slack's warm
    /// rotation, where the new connection opens before the old one closes).
    async fn close(&mut self) -> Result<(), SlackSocketSeamError>;
}

/// Real [`SlackSocketSeam`]: calls `slack.com/api/apps.connections.open` over
/// HTTPS, then opens (or reuses) one live WSS connection and reads off it
/// directly. Reconnecting is always the caller's job — this type never
/// reconnects itself, it just reflects "not connected" as an error once
/// [`SlackSocketSeam::close`] (or a failed [`SlackSocketSeam::connect`]) has
/// run.
pub struct TungsteniteSlackSocketSeam {
    http: reqwest::Client,
    socket: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    idle_read_timeout: Duration,
}

impl TungsteniteSlackSocketSeam {
    pub fn new() -> Self {
        Self::with_idle_read_timeout(IDLE_READ_TIMEOUT)
    }

    /// Same as [`Self::new`], but with a caller-supplied idle-read timeout in
    /// place of the production [`IDLE_READ_TIMEOUT`] — exists so a test can
    /// shrink it to a few milliseconds rather than waiting out the real
    /// production value to prove a silent peer is eventually reported as
    /// dead rather than hanging forever.
    fn with_idle_read_timeout(idle_read_timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            // Only errors on a broken TLS backend or resolver setup, never
            // on config values like a fixed timeout.
            .expect("slack socket seam http client with a fixed timeout must always build");
        Self { http, socket: None, idle_read_timeout }
    }
}

impl Default for TungsteniteSlackSocketSeam {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SlackSocketSeam for TungsteniteSlackSocketSeam {
    async fn connect(&mut self, app_token: &str) -> Result<String, SlackSocketSeamError> {
        let response = self
            .http
            .post(format!("{SLACK_API_BASE}/apps.connections.open"))
            .header("Authorization", format!("Bearer {app_token}"))
            .send()
            .await
            .map_err(|e| SlackSocketSeamError::ConnectionsOpen(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(SlackSocketSeamError::ConnectionsOpen(format!("unexpected HTTP status {status}")));
        }

        let body: serde_json::Value =
            response.json().await.map_err(|e| SlackSocketSeamError::ConnectionsOpen(e.to_string()))?;

        if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let error = body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown_error").to_string();
            return Err(SlackSocketSeamError::ConnectionsOpen(error));
        }

        let url = body
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SlackSocketSeamError::ConnectionsOpen("response missing url".to_string()))?
            .to_string();

        let (stream, _response) =
            tokio_tungstenite::connect_async(&url).await.map_err(|e| SlackSocketSeamError::Connect(e.to_string()))?;
        self.socket = Some(stream);

        Ok(url)
    }

    async fn send(&mut self, text: &str) -> Result<(), SlackSocketSeamError> {
        let socket = self.socket.as_mut().ok_or(SlackSocketSeamError::NotConnected)?;
        socket.send(Message::text(text.to_string())).await.map_err(|e| SlackSocketSeamError::Write(e.to_string()))
    }

    async fn recv(&mut self) -> Result<SocketFrame, SlackSocketSeamError> {
        loop {
            let socket = self.socket.as_mut().ok_or(SlackSocketSeamError::NotConnected)?;
            // Timed per individual poll, not around the whole method: a
            // healthy connection may legitimately answer several pings
            // before any text frame arrives, and each of those resets the
            // clock. Only a single poll going the full `idle_read_timeout`
            // with nothing at all — not even a ping — means the connection
            // is dead. See [`IDLE_READ_TIMEOUT`]'s doc for why this exists.
            match tokio::time::timeout(self.idle_read_timeout, socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => return Ok(SocketFrame::Text(text.to_string())),
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|e| SlackSocketSeamError::Write(e.to_string()))?;
                }
                Ok(Some(Ok(Message::Pong(_) | Message::Binary(_) | Message::Frame(_)))) => {}
                Ok(Some(Ok(Message::Close(frame)))) => {
                    return Err(SlackSocketSeamError::ClosedByPeer { code: frame.map(|f| u16::from(f.code)) });
                }
                Ok(Some(Err(e))) => return Err(SlackSocketSeamError::Read(e.to_string())),
                Ok(None) => return Err(SlackSocketSeamError::StreamEnded),
                Err(_elapsed) => return Err(SlackSocketSeamError::IdleTimeout),
            }
        }
    }

    async fn close(&mut self) -> Result<(), SlackSocketSeamError> {
        if let Some(mut socket) = self.socket.take() {
            // Best-effort: a peer that's already gone will error here too,
            // which is fine — the goal is that this seam never leaves a
            // handle to a half-open socket lying around for a caller to
            // (mistakenly) keep reading from, not that the close frame
            // itself always reaches an already-vanished peer.
            let _ = socket.close(None).await;
        }
        Ok(())
    }
}

/// In-memory [`SlackSocketSeam`] fake — scripts a `connect` result plus an
/// ordered queue of `recv` results, and records every `send`ed frame for
/// assertions. Exported unconditionally (not `#[cfg(test)]`-gated), mirroring
/// [`super::fake_seam::FakeSlackApiSeam`], so a future `runner`'s own tests
/// (and any cross-crate integration test) can drive the connect/dispatch
/// loop hermetically, without a live Slack workspace.
pub struct FakeSlackSocketSeam {
    connect_result: Result<String, SlackSocketSeamError>,
    queued_frames: VecDeque<Result<SocketFrame, SlackSocketSeamError>>,
    sent: Vec<String>,
    closed: bool,
}

impl FakeSlackSocketSeam {
    /// Scripts a `connect` outcome and an ordered list of `recv` outcomes,
    /// each independently either a frame or an error — e.g. a scripted
    /// `ClosedByPeer` partway through a queue simulates a mid-stream drop.
    pub fn new(
        connect_result: Result<String, SlackSocketSeamError>,
        frames: Vec<Result<SocketFrame, SlackSocketSeamError>>,
    ) -> Self {
        Self { connect_result, queued_frames: frames.into(), sent: Vec::new(), closed: false }
    }

    /// Convenience constructor for the common case: `connect` succeeds with
    /// `url`, and every queued item is a successful text frame.
    pub fn connects_to(url: impl Into<String>, frames: Vec<SocketFrame>) -> Self {
        Self::new(Ok(url.into()), frames.into_iter().map(Ok).collect())
    }

    /// Every frame handed to [`SlackSocketSeam::send`] so far, in call order.
    pub fn sent_frames(&self) -> &[String] {
        &self.sent
    }

    /// Whether [`SlackSocketSeam::close`] has been called at least once.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

#[async_trait]
impl SlackSocketSeam for FakeSlackSocketSeam {
    async fn connect(&mut self, _app_token: &str) -> Result<String, SlackSocketSeamError> {
        self.connect_result.clone()
    }

    async fn send(&mut self, text: &str) -> Result<(), SlackSocketSeamError> {
        self.sent.push(text.to_string());
        Ok(())
    }

    async fn recv(&mut self) -> Result<SocketFrame, SlackSocketSeamError> {
        self.queued_frames.pop_front().unwrap_or(Err(SlackSocketSeamError::StreamEnded))
    }

    async fn close(&mut self) -> Result<(), SlackSocketSeamError> {
        self.closed = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_returns_the_url_from_apps_connections_open() {
        let mut seam = FakeSlackSocketSeam::connects_to("wss://example.slack.com/socket", vec![]);

        let url = seam.connect("xapp-fake").await.expect("connect succeeds");

        assert_eq!(url, "wss://example.slack.com/socket");
    }

    #[tokio::test]
    async fn connect_surfaces_a_scripted_connections_open_failure() {
        let mut seam =
            FakeSlackSocketSeam::new(Err(SlackSocketSeamError::ConnectionsOpen("invalid_auth".to_string())), vec![]);

        let err = seam.connect("xapp-bad").await.expect_err("bad app token fails");

        assert!(matches!(err, SlackSocketSeamError::ConnectionsOpen(msg) if msg == "invalid_auth"));
    }

    #[tokio::test]
    async fn recv_yields_queued_frames_in_order() {
        let mut seam = FakeSlackSocketSeam::connects_to(
            "wss://example.slack.com/socket",
            vec![SocketFrame::Text("first".to_string()), SocketFrame::Text("second".to_string())],
        );
        seam.connect("xapp-fake").await.expect("connect succeeds");

        assert_eq!(seam.recv().await.expect("first frame"), SocketFrame::Text("first".to_string()));
        assert_eq!(seam.recv().await.expect("second frame"), SocketFrame::Text("second".to_string()));
    }

    #[tokio::test]
    async fn recv_past_the_last_queued_frame_reports_stream_ended() {
        let mut seam = FakeSlackSocketSeam::connects_to("wss://example.slack.com/socket", vec![]);
        seam.connect("xapp-fake").await.expect("connect succeeds");

        let err = seam.recv().await.expect_err("no frames were queued");

        assert!(matches!(err, SlackSocketSeamError::StreamEnded));
    }

    #[tokio::test]
    async fn sent_frames_are_captured_in_call_order() {
        let mut seam = FakeSlackSocketSeam::connects_to("wss://example.slack.com/socket", vec![]);
        seam.connect("xapp-fake").await.expect("connect succeeds");

        seam.send(r#"{"envelope_id":"1"}"#).await.expect("send succeeds");
        seam.send(r#"{"envelope_id":"2"}"#).await.expect("send succeeds");

        assert_eq!(
            seam.sent_frames(),
            &[r#"{"envelope_id":"1"}"#.to_string(), r#"{"envelope_id":"2"}"#.to_string()]
        );
    }

    #[tokio::test]
    async fn close_is_idempotent_and_marks_the_fake_closed() {
        let mut seam = FakeSlackSocketSeam::connects_to("wss://example.slack.com/socket", vec![]);

        seam.close().await.expect("close succeeds");
        seam.close().await.expect("close succeeds again");

        assert!(seam.is_closed());
    }

    /// A connection the network has silently dropped — no close frame, no
    /// reset, just nothing ever again — must eventually be reported as dead
    /// rather than left to hang forever. Exercises the real
    /// [`TungsteniteSlackSocketSeam`] (not the fake, which has no internal
    /// read loop to time) against a real loopback WebSocket peer that
    /// completes the handshake and then goes deliberately silent.
    #[tokio::test]
    async fn recv_reports_idle_timeout_when_the_peer_goes_silent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            let _ws = tokio_tungstenite::accept_async(stream).await.expect("complete server handshake");
            // Deliberately never sends anything and never closes — the
            // silent-drop scenario this test proves we recover from.
            std::future::pending::<()>().await;
        });

        let (ws_stream, _response) =
            tokio_tungstenite::connect_async(format!("ws://{addr}")).await.expect("client connects");
        let mut seam = TungsteniteSlackSocketSeam::with_idle_read_timeout(Duration::from_millis(50));
        seam.socket = Some(ws_stream);

        let err = seam.recv().await.expect_err("a silent peer must time out, not hang forever");
        assert!(matches!(err, SlackSocketSeamError::IdleTimeout), "expected IdleTimeout, got {err:?}");

        server.abort();
    }
}
