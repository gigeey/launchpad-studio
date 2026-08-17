//! Lazy, cached REST resolution of a Discord channel's static shape:
//! whether it's a THREAD, its parent channel, and — for a thread — the user
//! id that created it.
//!
//! `MESSAGE_CREATE` never carries a channel's `type`, so nothing inbound can
//! tell a normal guild text channel from a thread without a side lookup.
//! That shape never changes over a channel's life (a thread doesn't change
//! parent or creator after it's made), so [`ChannelMetaCache`] never
//! invalidates an entry once resolved — it only ever needs to look one up
//! once. [`ChannelMetaSeam`] isolates the actual `GET /channels/{channel_id}`
//! call the same way [`super::outbound_seam::DiscordSendSeam`] isolates the
//! outbound REST send: [`resolve_channel_meta`]'s cache-hit short-circuit
//! and error-swallowing default are exercised in this module's tests against
//! a fake seam, never a live network call.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use tracing::warn;

/// Discord channel `type` values that mark a channel as a thread, as
/// opposed to a guild text/voice/category/forum channel: announcement
/// thread, public thread, private thread.
fn is_thread_channel_type(channel_type: u8) -> bool {
    matches!(channel_type, 10 | 11 | 12)
}

/// A Discord channel's static shape: whether it's a thread, its parent
/// channel (for a thread, the channel it was created under), and — for a
/// thread specifically — the id of the user who created it. `owner_id` is
/// what lets a caller answer "did the bot itself start this thread."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMeta {
    pub id: String,
    pub is_thread: bool,
    pub parent_id: Option<String>,
    pub owner_id: Option<String>,
}

impl ChannelMeta {
    /// The value [`resolve_channel_meta`] returns when the channel object
    /// couldn't be resolved — treats an unresolvable channel as a plain,
    /// non-thread channel rather than propagating the lookup failure up to
    /// the inbound message path.
    fn unresolved(channel_id: &str) -> Self {
        Self { id: channel_id.to_string(), is_thread: false, parent_id: None, owner_id: None }
    }
}

/// The subset of Discord's channel object this lookup needs.
#[derive(Debug, Deserialize)]
struct RawChannel {
    id: String,
    #[serde(rename = "type")]
    channel_type: u8,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    owner_id: Option<String>,
}

impl From<RawChannel> for ChannelMeta {
    fn from(raw: RawChannel) -> Self {
        Self {
            id: raw.id,
            is_thread: is_thread_channel_type(raw.channel_type),
            parent_id: raw.parent_id,
            owner_id: raw.owner_id,
        }
    }
}

/// Bound on [`ChannelMetaCache`]'s entry count. A channel's type/parent/
/// owner never change, so nothing here is ever invalidated on a hit — only
/// capped, so a long-lived bot that has touched many distinct
/// channels/threads over its lifetime doesn't grow this without limit.
/// Oldest-first eviction on overflow, the same bound `super::session::SeenMessageIds`
/// uses for its own dedup set: 512 is comfortably larger than the number of
/// channels/threads any one binding is realistically active in at once.
const CHANNEL_META_CAPACITY: usize = 512;

#[derive(Default)]
struct ChannelMetaCacheState {
    order: VecDeque<String>,
    by_id: HashMap<String, ChannelMeta>,
}

/// Cloneable, in-memory `channel_id -> ChannelMeta` cache, shared across
/// every connection one binding opens over its lifetime. Unlike
/// [`super::InFlightChannels`] (whose caller wraps it in an `Arc` from the
/// outside), this type carries its own internal `Arc` so a plain `.clone()`
/// is itself a cheap, shareable handle — matching how [`reqwest::Client`]
/// (this module's other per-transport shared handle) is already threaded
/// through this same plumbing.
#[derive(Clone, Default)]
pub struct ChannelMetaCache {
    state: Arc<Mutex<ChannelMetaCacheState>>,
}

impl ChannelMetaCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, channel_id: &str) -> Option<ChannelMeta> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).by_id.get(channel_id).cloned()
    }

    /// Records `meta`, evicting the oldest entry first if the cache is
    /// already at [`CHANNEL_META_CAPACITY`]. [`resolve_channel_meta`] never
    /// calls this for a failed lookup — see its doc comment.
    fn insert(&self, meta: ChannelMeta) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.by_id.contains_key(&meta.id) {
            state.order.push_back(meta.id.clone());
            if state.order.len() > CHANNEL_META_CAPACITY {
                if let Some(oldest) = state.order.pop_front() {
                    state.by_id.remove(&oldest);
                }
            }
        }
        state.by_id.insert(meta.id.clone(), meta);
    }
}

#[derive(Debug, Error)]
enum ChannelMetaSeamError {
    #[error("discord channel lookup failed: {0}")]
    Request(String),
    #[error("discord channel lookup returned a non-success status: {status}")]
    Status { status: u16 },
    #[error("discord channel lookup response did not parse: {0}")]
    Parse(String),
}

/// The network boundary a single `GET /channels/{channel_id}` call goes
/// through — the analogue of [`super::outbound_seam::DiscordSendSeam`] for
/// this lookup. [`ReqwestChannelMetaSeam`] is the only implementation that
/// actually calls the Discord REST API; this module's tests drive
/// [`resolve_via_seam`] against an in-memory fake instead, so the cache-hit
/// short-circuit and the never-cache-a-failure rule are provable without a
/// live network call.
#[async_trait]
trait ChannelMetaSeam: Send + Sync {
    async fn fetch(&self, token: &str, channel_id: &str) -> Result<ChannelMeta, ChannelMetaSeamError>;
}

/// Real [`ChannelMetaSeam`]: fetches straight from the Discord REST API,
/// authenticated the same way `runner::resolve_dm_member_roles` and
/// `outbound_seam::ReqwestSendSeam` already are — an `Authorization: Bot
/// <token>` header, never the token in a log line.
struct ReqwestChannelMetaSeam {
    http: reqwest::Client,
}

#[async_trait]
impl ChannelMetaSeam for ReqwestChannelMetaSeam {
    async fn fetch(&self, token: &str, channel_id: &str) -> Result<ChannelMeta, ChannelMetaSeamError> {
        let url = format!("https://discord.com/api/v10/channels/{channel_id}");
        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bot {token}"))
            .send()
            .await
            .map_err(|e| ChannelMetaSeamError::Request(e.to_string()))?;
        if !response.status().is_success() {
            return Err(ChannelMetaSeamError::Status { status: response.status().as_u16() });
        }
        let raw: RawChannel = response.json().await.map_err(|e| ChannelMetaSeamError::Parse(e.to_string()))?;
        Ok(ChannelMeta::from(raw))
    }
}

/// Resolves `channel_id`'s [`ChannelMeta`], preferring `cache` over a REST
/// call. Mirrors `runner::resolve_dm_member_roles`'s error handling exactly:
/// a network error, non-2xx status, or unparseable body logs a warning and
/// returns [`ChannelMeta::unresolved`] rather than propagating a failure —
/// an unresolvable channel is treated as a plain, non-thread channel rather
/// than blocking inbound message handling on this lookup. A failed lookup
/// is never cached, so the next message on the same channel retries it.
pub async fn resolve_channel_meta(
    http: &reqwest::Client,
    token: &str,
    cache: &ChannelMetaCache,
    channel_id: &str,
) -> ChannelMeta {
    let seam = ReqwestChannelMetaSeam { http: http.clone() };
    resolve_via_seam(&seam, cache, token, channel_id).await
}

/// The seam-generic core [`resolve_channel_meta`] wraps around the real
/// [`ReqwestChannelMetaSeam`] — split out so tests can drive it against a
/// fake seam directly.
async fn resolve_via_seam(
    seam: &dyn ChannelMetaSeam,
    cache: &ChannelMetaCache,
    token: &str,
    channel_id: &str,
) -> ChannelMeta {
    if let Some(meta) = cache.get(channel_id) {
        return meta;
    }
    match seam.fetch(token, channel_id).await {
        Ok(meta) => {
            cache.insert(meta.clone());
            meta
        }
        Err(e) => {
            warn!(channel_id = %channel_id, "DiscordTransport: channel metadata lookup failed: {e}");
            ChannelMeta::unresolved(channel_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    // --- is_thread_channel_type ---

    #[test]
    fn guild_text_channel_is_not_a_thread() {
        assert!(!is_thread_channel_type(0));
    }

    #[test]
    fn guild_voice_channel_is_not_a_thread() {
        assert!(!is_thread_channel_type(2));
    }

    #[test]
    fn announcement_thread_is_a_thread() {
        assert!(is_thread_channel_type(10));
    }

    #[test]
    fn public_thread_is_a_thread() {
        assert!(is_thread_channel_type(11));
    }

    #[test]
    fn private_thread_is_a_thread() {
        assert!(is_thread_channel_type(12));
    }

    #[test]
    fn forum_channel_is_not_a_thread() {
        assert!(!is_thread_channel_type(15));
    }

    // --- RawChannel deserialization ---

    #[test]
    fn deserializes_a_public_thread_channel_object() {
        let json = r#"{
            "id": "111",
            "type": 11,
            "parent_id": "222",
            "owner_id": "333"
        }"#;
        let raw: RawChannel = serde_json::from_str(json).expect("thread channel object must parse");
        let meta = ChannelMeta::from(raw);
        assert_eq!(
            meta,
            ChannelMeta {
                id: "111".to_string(),
                is_thread: true,
                parent_id: Some("222".to_string()),
                owner_id: Some("333".to_string()),
            }
        );
    }

    #[test]
    fn deserializes_a_normal_text_channel_object_with_no_parent_or_owner() {
        let json = r#"{
            "id": "444",
            "type": 0
        }"#;
        let raw: RawChannel = serde_json::from_str(json).expect("text channel object must parse");
        let meta = ChannelMeta::from(raw);
        assert_eq!(
            meta,
            ChannelMeta { id: "444".to_string(), is_thread: false, parent_id: None, owner_id: None }
        );
    }

    // --- resolve_via_seam / ChannelMetaCache ---

    /// Fake [`ChannelMetaSeam`] that counts calls and returns a fixed
    /// result instead of hitting the network, so cache short-circuiting can
    /// be asserted directly rather than inferred from timing.
    struct FakeSeam {
        calls: StdMutex<u32>,
        result: Result<ChannelMeta, String>,
    }

    impl FakeSeam {
        fn ok(meta: ChannelMeta) -> Self {
            Self { calls: StdMutex::new(0), result: Ok(meta) }
        }

        fn failing() -> Self {
            Self { calls: StdMutex::new(0), result: Err("simulated failure".to_string()) }
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    #[async_trait]
    impl ChannelMetaSeam for FakeSeam {
        async fn fetch(&self, _token: &str, _channel_id: &str) -> Result<ChannelMeta, ChannelMetaSeamError> {
            *self.calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            self.result.clone().map_err(ChannelMetaSeamError::Request)
        }
    }

    #[tokio::test]
    async fn cache_hit_resolves_without_invoking_the_seam_again() {
        let seam = FakeSeam::ok(ChannelMeta {
            id: "1".to_string(),
            is_thread: true,
            parent_id: Some("2".to_string()),
            owner_id: Some("3".to_string()),
        });
        let cache = ChannelMetaCache::new();

        let first = resolve_via_seam(&seam, &cache, "token", "1").await;
        assert_eq!(seam.call_count(), 1, "a cache miss must call the seam exactly once");

        let second = resolve_via_seam(&seam, &cache, "token", "1").await;
        assert_eq!(seam.call_count(), 1, "a cache hit must not invoke the seam a second time");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn a_failed_lookup_returns_the_unresolved_default_and_is_never_cached() {
        let seam = FakeSeam::failing();
        let cache = ChannelMetaCache::new();

        let meta = resolve_via_seam(&seam, &cache, "token", "9").await;
        assert_eq!(meta, ChannelMeta::unresolved("9"));
        assert_eq!(seam.call_count(), 1);

        // A failure must never populate the cache, so the very next call
        // retries the lookup rather than serving a cached failure forever.
        let _ = resolve_via_seam(&seam, &cache, "token", "9").await;
        assert_eq!(seam.call_count(), 2, "a failed lookup must never be cached");
    }

    #[test]
    fn cache_evicts_the_oldest_entry_once_past_capacity() {
        let cache = ChannelMetaCache::new();
        for i in 0..=CHANNEL_META_CAPACITY {
            cache.insert(ChannelMeta {
                id: format!("channel-{i}"),
                is_thread: false,
                parent_id: None,
                owner_id: None,
            });
        }
        assert!(cache.get("channel-0").is_none(), "the oldest entry must be evicted once capacity overflows");
        assert!(
            cache.get(&format!("channel-{CHANNEL_META_CAPACITY}")).is_some(),
            "the most recently inserted entry must survive"
        );
    }
}
