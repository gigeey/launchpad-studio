//! Shared atomic provisioning of a channel binding's dedicated bridge
//! thread.
//!
//! Any HTTP handler that can flip a [`ChannelBinding`]'s `enabled` bit — the
//! agent-profile `PUT` route and each channel's own secret-setting route
//! (e.g. Telegram's token endpoint) — must provision the binding's
//! `bridge_thread_id` in the same request that enables it. Before this
//! helper existed, only the profile `PUT` route did that provisioning, so a
//! bot enabled purely through the Telegram token endpoint silently never
//! started until some later, unrelated profile save happened to trigger
//! provisioning.

use ao_engine::AppState;
use ao_protocol::agent::{ChannelBinding, ChannelKindConfig};
use ao_protocol::thread::ChannelBridgeOrigin;

use crate::error::AppError;

/// If `binding` is enabled and has no `bridge_thread_id` yet, creates a
/// fresh dedicated thread for it and sets `bridge_thread_id` on `binding` in
/// place. No-op otherwise (already provisioned, not enabled, or — as of the
/// per-conversation minting phase — a Discord, Telegram, or Email binding,
/// none of which ever gets one at all; see below).
///
/// The created thread is stamped with a [`ChannelBridgeOrigin`] naming this
/// binding, so `Thread::channel_origin` — not just the reverse-lookup via
/// `bridge_thread_id` — recognizes it as a bridge thread. This matters
/// beyond this one thread: it's what lets a *different* creation path (e.g.
/// Slack's per-conversation `resolve_bridge_thread`, Discord's
/// `resolve_discord_conversation_thread`, Telegram's
/// `resolve_telegram_conversation_thread`, or Email's
/// `resolve_email_conversation_thread`, none of which ever touch
/// `bridge_thread_id` at all) be recognized the same way.
pub async fn provision_bridge_thread(
    state: &AppState,
    agent_id: &str,
    binding: &mut ChannelBinding,
) -> Result<(), AppError> {
    if !binding.enabled || binding.bridge_thread_id.is_some() {
        return Ok(());
    }
    // Discord, Telegram, and Email each mint a fresh per-conversation bridge
    // thread on demand for every distinct conversation they see (see
    // `ao_engine::channels::discord::runner::resolve_discord_conversation_thread`,
    // `ao_engine::telegram::transport::resolve_telegram_conversation_thread`,
    // and `ao_engine::channels::email::resolve_email_conversation_thread`)
    // rather than routing every conversation through one
    // eagerly-provisioned thread — the same shape Slack has always used —
    // so there is nothing to provision here at bind-enable time.
    // `binding.bridge_thread_id` is left `None` permanently; a binding
    // provisioned before this change keeps its legacy thread as a viewable,
    // no-longer-written-to artifact rather than having it reassigned.
    if matches!(
        binding.kind_config,
        ChannelKindConfig::Discord { .. } | ChannelKindConfig::Telegram { .. } | ChannelKindConfig::Email { .. }
    ) {
        return Ok(());
    }

    let title = bridge_thread_title(binding);
    let mut thread = state.persistence.threads.build_fresh_thread(agent_id, title);
    thread.channel_origin = Some(ChannelBridgeOrigin {
        kind: binding.kind,
        binding_id: binding.binding_id.clone(),
    });
    let created = state.persistence.threads.create(thread).await?;
    binding.bridge_thread_id = Some(created.id);
    Ok(())
}

/// Static title to stamp on a freshly-minted bridge thread, or `None` to
/// leave it untitled so the auto-title derived from the first inbound
/// message (`set_auto_title_if_unset`) — and ultimately the frontend's
/// channel-kind-label fallback — can take over instead.
///
/// Discord, Telegram, and Email no longer reach this function at all —
/// `provision_bridge_thread` returns early for all three above, now that
/// each mints a fresh per-conversation thread on demand the same way Slack
/// always has; their arms below are kept only because the match must stay
/// exhaustive over `ChannelKindConfig`. Slack is the other kind that flows
/// through this same function — via
/// `upsert_slack_channel`/`set_slack_channel_secret` — but the thread it
/// provisions here is a placeholder never used for real message routing;
/// Slack's actual per-conversation threads are minted lazily, on first
/// contact, by `resolve_bridge_thread`, which already leaves `title` unset
/// and lets the first message seed `auto_title` itself. So Slack keeps its
/// static placeholder title, unchanged.
fn bridge_thread_title(binding: &ChannelBinding) -> Option<String> {
    match &binding.kind_config {
        ChannelKindConfig::Telegram { .. } => None, // unreachable in practice; kept for match exhaustiveness
        ChannelKindConfig::Email { .. } => None, // unreachable in practice; kept for match exhaustiveness
        ChannelKindConfig::Discord { .. } => None, // unreachable in practice; kept for match exhaustiveness
        ChannelKindConfig::Slack { .. } => Some("💬 Slack".to_string()),
    }
}
