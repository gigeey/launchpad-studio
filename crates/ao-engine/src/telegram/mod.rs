//! Telegram Bot API integration.
//!
//! `client` is the thin Bot API wrapper (`getMe`, `getUpdates`,
//! `sendMessage`). `transport` is Telegram's implementation of the
//! channel-agnostic [`crate::channels::ChannelTransport`] trait — the
//! inbound long-poll loop, one task per agent with an enabled, provisioned
//! Telegram binding, routing inbound messages onto that binding's dedicated
//! bridge thread. `bridge` is [`ChannelBridge`], the channel-agnostic
//! supervisor that starts/stops those tasks (Telegram today; other channel
//! kinds register their own transport later). `outbound` is the reverse
//! path: a single shared `EventBus` observer that relays a bridge thread's
//! finished reply back to the chat that triggered it, rendering the agent's
//! CommonMark-ish reply into Telegram's HTML subset via `html` first —
//! Telegram-specific for now, since no other channel needs an automatic
//! relay yet. Chat-linking security is reject-all by default (an empty
//! allow-list delivers nothing); the only way in is `/start <code>` matching
//! a live pairing code, handled in `transport` before the allow-list gate.

mod bridge;
mod client;
mod html;
mod outbound;
mod transport;

pub use bridge::ChannelBridge;
pub use client::{
    TelegramApiError, TelegramBotInfo, TelegramChat, TelegramChatType, TelegramClient,
    TelegramMessage, TelegramMessageEntity, TelegramMessageEntityType, TelegramUpdate,
    TelegramUser,
};
pub use transport::TelegramTransport;

/// Shared by every submodule's tests that redirect the process-wide
/// `LAUNCHPAD_TELEGRAM_*`/data-root env vars at a local mock server or temp
/// dir (`client`'s API-base override, `bridge`'s and `outbound`'s data-root
/// + token-store-fallback overrides).
///
/// Delegates to [`crate::plugin_paths::tests::ENV_LOCK`] rather than
/// defining a module-local mutex: `LAUNCHPAD_STUDIO_DATA_DIR` is mutated by
/// tests all over this crate (`lib.rs`, `agent_runner`, `plugin_paths`), and
/// that lock's own contract is to be the *one* lock for
/// the var — a second, uncoordinated mutex here still lets those tests race
/// this module's under `cargo test`'s default parallel test threads, since
/// two different mutexes provide no mutual exclusion with each other.
#[cfg(test)]
pub(crate) mod test_env {
    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::plugin_paths::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}
