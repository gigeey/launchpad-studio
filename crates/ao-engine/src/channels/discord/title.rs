//! Pure cleanup of Discord's wire markup out of an inbound message's raw
//! content, ahead of [`ao_protocol::thread::derive_auto_title`] (invoked
//! generically by `crate::channels::submit_inbound_message`, not here — see
//! that function's doc for why the shared seam stays channel-agnostic).
//! Discord's mention tokens carry only a bare snowflake id, never a
//! display name (unlike Slack's `<@U0123|alice>`), so [`clean_discord_markup`]
//! has nothing legible to keep for a user/channel/role mention and drops it
//! outright; a custom or animated emoji token is the one case with a
//! human-readable name worth keeping, so it's rewritten to its
//! `:shortcode:` form instead of being dropped.

use std::sync::LazyLock;

use regex::Regex;

/// `<@123>`, `<@!123>` (nickname mention), or `<@&123>` (role mention) — all
/// three carry only a snowflake id Discord's own client resolves to a name
/// client-side, so a title-cleaner has no display text to fall back on and
/// simply drops the token.
static MENTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<@[!&]?\d+>").expect("mention regex should compile"));

/// `<#123>` — a channel reference, same bare-id shape as a mention.
static CHANNEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<#\d+>").expect("channel regex should compile"));

/// `<:name:123>` or `<a:name:123>` (animated) — a custom emoji. Unlike a
/// mention, this carries a human-readable `name` worth keeping, so it's
/// rewritten to Discord's own `:name:` shortcode form rather than dropped.
static CUSTOM_EMOJI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<a?:(\w+):\d+>").expect("custom emoji regex should compile"));

/// Strips Discord's markup tokens down to plain, human-legible text. Cheap
/// and regex-based on purpose, mirroring `slack::title::clean_slack_markup`
/// — good enough to title a conversation, not a full markdown renderer.
pub fn clean_discord_markup(text: &str) -> String {
    let cleaned = MENTION_RE.replace_all(text, "");
    let cleaned = CHANNEL_RE.replace_all(&cleaned, "");
    let cleaned = CUSTOM_EMOJI_RE.replace_all(&cleaned, |caps: &regex::Captures| format!(":{}:", &caps[1]));
    cleaned.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(clean_discord_markup("can you help with the deploy?"), "can you help with the deploy?");
    }

    #[test]
    fn drops_a_bare_user_mention() {
        assert_eq!(clean_discord_markup("<@123456> can you help?"), " can you help?");
    }

    #[test]
    fn drops_a_nickname_mention() {
        assert_eq!(clean_discord_markup("<@!123456> can you help?"), " can you help?");
    }

    #[test]
    fn drops_a_role_mention() {
        assert_eq!(clean_discord_markup("hey <@&987654>, ping the team"), "hey , ping the team");
    }

    #[test]
    fn drops_a_channel_reference() {
        assert_eq!(clean_discord_markup("see <#555111> for details"), "see  for details");
    }

    #[test]
    fn rewrites_a_custom_emoji_to_its_shortcode() {
        assert_eq!(clean_discord_markup("nice work <:tada:1>"), "nice work :tada:");
    }

    #[test]
    fn rewrites_an_animated_emoji_to_its_shortcode() {
        assert_eq!(clean_discord_markup("nice work <a:party:42>"), "nice work :party:");
    }

    #[test]
    fn markup_only_message_cleans_to_blank_and_derives_no_title() {
        let cleaned = clean_discord_markup("<@123456>");
        assert_eq!(ao_protocol::thread::derive_auto_title(&cleaned), None);
    }
}
