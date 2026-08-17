//! The network I/O seam [`super::runner`]'s connect/reconnect state machine
//! drives each cycle — the Discord analogue of
//! [`crate::channels::email::imap_seam::MailSource`].
//!
//! [`GatewaySeam`] exists so the reconnect/heartbeat/dispatch state machine
//! in `runner` is unit-testable against a scripted fake without a live
//! gateway socket. [`TungsteniteGatewaySeam`] is the only implementation
//! that actually opens a WSS connection, via `tokio-tungstenite`.
//!
//! [`GatewaySeam::next_event`] owns both the socket read *and* the
//! heartbeat clock internally, racing them against each other and returning
//! [`GatewayEvent::HeartbeatSent`] when the timer fires instead of a raw
//! frame. Folding the timer into the same `&mut self` call this way (rather
//! than a caller-side `select!` between "read a frame" and "send a
//! heartbeat") sidesteps needing two independent mutable handles onto the
//! same socket — sending the heartbeat frame happens synchronously inside
//! this one call, using the socket handle already borrowed for the read
//! race, never a second call back into `self`.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::protocol::{self, GatewayEvent, ProtocolError};

/// Heartbeat interval fallback used only if a caller somehow asks this seam
/// to heartbeat before a `HELLO` armed one — shouldn't happen in practice
/// (the state machine always arms the real interval off `HELLO` first), but
/// keeps `next_event` from panicking on an `unwrap` if it ever does.
const HEARTBEAT_INTERVAL_FALLBACK: Duration = Duration::from_secs(45);

#[derive(Debug, Error)]
pub enum GatewaySeamError {
    #[error("gateway connect failed: {0}")]
    Connect(String),
    #[error("gateway socket is not connected")]
    NotConnected,
    #[error("gateway socket read failed: {0}")]
    Read(String),
    #[error("gateway socket write failed: {0}")]
    Write(String),
    #[error("gateway frame did not parse: {0}")]
    Parse(#[from] ProtocolError),
    #[error("gateway connection was closed by the peer (code {code:?})")]
    ClosedByPeer { code: Option<u16> },
    #[error("gateway connection ended without a close frame")]
    StreamEnded,
}

/// The network I/O boundary for one Gateway connection's lifetime — opened
/// by [`Self::connect`], read via [`Self::next_event`], and always ended via
/// [`Self::close`] before a caller opens the next one (see the module doc on
/// [`super::runner`] for why the close-before-reopen ordering is mandatory).
#[async_trait]
pub trait GatewaySeam: Send {
    async fn connect(&mut self, url: &str) -> Result<(), GatewaySeamError>;

    /// Explicitly closes the socket. Idempotent — closing an already-closed
    /// or never-opened seam is a no-op, not an error, so callers can call it
    /// unconditionally on every reconnect path.
    async fn close(&mut self) -> Result<(), GatewaySeamError>;

    /// Arms (or re-arms, e.g. after a fresh `HELLO` on a new connection) the
    /// periodic heartbeat timer at `interval`. Per the Gateway spec, only
    /// the first heartbeat after arming is jittered
    /// (`interval * random(0,1)`); every heartbeat after that fires exactly
    /// `interval` after the previous one.
    fn arm_heartbeat(&mut self, interval: Duration);

    /// Waits for whichever happens first: the next inbound frame, or the
    /// heartbeat timer (once armed) coming due — in which case this sends
    /// `op1 HEARTBEAT` (carrying `last_seq`) itself and returns
    /// [`GatewayEvent::HeartbeatSent`] rather than blocking further; the
    /// caller re-calls `next_event` immediately after to keep reading.
    async fn next_event(&mut self, last_seq: Option<u64>) -> Result<GatewayEvent, GatewaySeamError>;

    /// Sends one JSON Gateway payload (`IDENTIFY`, `RESUME`, an out-of-cycle
    /// heartbeat reply to `op1`) as a text frame.
    async fn send_json(&mut self, payload: &serde_json::Value) -> Result<(), GatewaySeamError>;
}

/// Real [`GatewaySeam`]: opens (or reuses) one live WSS connection and reads
/// off it directly. Reconnecting is always the caller's job — this type
/// never reconnects itself, it just reflects "not connected" as an error
/// once [`GatewaySeam::close`] (or a failed [`GatewaySeam::connect`]) has run.
pub struct TungsteniteGatewaySeam {
    socket: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    heartbeat_interval: Option<Duration>,
    next_heartbeat_at: Option<Instant>,
}

impl TungsteniteGatewaySeam {
    pub fn new() -> Self {
        Self { socket: None, heartbeat_interval: None, next_heartbeat_at: None }
    }
}

impl Default for TungsteniteGatewaySeam {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GatewaySeam for TungsteniteGatewaySeam {
    async fn connect(&mut self, url: &str) -> Result<(), GatewaySeamError> {
        let (stream, _response) =
            tokio_tungstenite::connect_async(url).await.map_err(|e| GatewaySeamError::Connect(e.to_string()))?;
        self.socket = Some(stream);
        self.heartbeat_interval = None;
        self.next_heartbeat_at = None;
        Ok(())
    }

    async fn close(&mut self) -> Result<(), GatewaySeamError> {
        if let Some(mut socket) = self.socket.take() {
            // Best-effort: a peer that's already gone will error here too,
            // which is fine — the goal is that this seam never leaves a
            // handle to a half-open socket lying around for a caller to
            // (mistakenly) keep reading from, not that the close frame
            // itself always reaches an already-vanished peer.
            let _ = socket.close(None).await;
        }
        self.heartbeat_interval = None;
        self.next_heartbeat_at = None;
        Ok(())
    }

    fn arm_heartbeat(&mut self, interval: Duration) {
        self.heartbeat_interval = Some(interval);
        self.next_heartbeat_at = Some(Instant::now() + interval.mul_f64(super::jitter_unit()));
    }

    async fn next_event(&mut self, last_seq: Option<u64>) -> Result<GatewayEvent, GatewaySeamError> {
        loop {
            let heartbeat_armed = self.next_heartbeat_at.is_some();
            // Only polled when `heartbeat_armed` (via the `if` guard below);
            // `Instant::now()` is a harmless placeholder deadline otherwise.
            let deadline = self.next_heartbeat_at.unwrap_or_else(Instant::now);
            let socket = self.socket.as_mut().ok_or(GatewaySeamError::NotConnected)?;

            tokio::select! {
                _ = tokio::time::sleep_until(deadline), if heartbeat_armed => {
                    let interval = self.heartbeat_interval.unwrap_or(HEARTBEAT_INTERVAL_FALLBACK);
                    self.next_heartbeat_at = Some(Instant::now() + interval);
                    let payload = protocol::heartbeat_payload(last_seq);
                    socket
                        .send(Message::text(payload.to_string()))
                        .await
                        .map_err(|e| GatewaySeamError::Write(e.to_string()))?;
                    return Ok(GatewayEvent::HeartbeatSent);
                }
                msg = socket.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => return Ok(protocol::parse_gateway_payload(text.as_str())?),
                        Some(Ok(Message::Ping(payload))) => {
                            socket
                                .send(Message::Pong(payload))
                                .await
                                .map_err(|e| GatewaySeamError::Write(e.to_string()))?;
                        }
                        Some(Ok(Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => {}
                        Some(Ok(Message::Close(frame))) => {
                            return Err(GatewaySeamError::ClosedByPeer { code: frame.map(|f| u16::from(f.code)) });
                        }
                        Some(Err(e)) => return Err(GatewaySeamError::Read(e.to_string())),
                        None => return Err(GatewaySeamError::StreamEnded),
                    }
                }
            }
        }
    }

    async fn send_json(&mut self, payload: &serde_json::Value) -> Result<(), GatewaySeamError> {
        let socket = self.socket.as_mut().ok_or(GatewaySeamError::NotConnected)?;
        socket.send(Message::text(payload.to_string())).await.map_err(|e| GatewaySeamError::Write(e.to_string()))
    }
}
