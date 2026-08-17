//! Pure derivation of a fresh Slack bridge thread's `auto_title` from the
//! raw text of the inbound message that minted it — see
//! `super::runner::resolve_bridge_thread`'s `Ok(None)` (fresh-thread) branch,
//! the only caller. Slack's `mrkdwn` wire format wraps mentions, channel
//! references, and links in `<...>` markup and escapes a handful of HTML
//! entities in `text`; [`derive_slack_channel_title`] cleans that down to
//! plain text first, then defers to [`ao_protocol::thread::derive_auto_title`]
//! for the exact same whitespace-collapse + truncation rule every other
//! auto-title path in the app already shares (`ao-server/src/routes/messages.rs`,
//! the `RenameThread` tool) — so a channel thread's title is trimmed by the
//! same yardstick a normal thread's is, not a second, subtly different one.

use std::sync::LazyLock;

use regex::Regex;

use ao_protocol::thread::derive_auto_title;

/// `<@U0123>` or `<@U0123|alice>` — a user mention. The optional `|name`
/// label is captured so the cleaner can keep the human-readable half and
/// drop the raw, meaningless-in-a-title user id either way.
static USER_MENTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<@([A-Za-z0-9]+)(?:\|([^>]*))?>").expect("user mention regex should compile"));

/// `<#C0123>` or `<#C0123|general>` — a channel reference.
static CHANNEL_MENTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<#([A-Za-z0-9]+)(?:\|([^>]*))?>").expect("channel mention regex should compile"));

/// `<https://x>` or `<https://x|label>` — a link. Unlike mentions/channel
/// refs (which only ever render as a name), Slack always wraps a link in
/// angle brackets even when it has no display-text half, so the bare URL
/// itself is the fallback.
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(https?://[^|>]+)(?:\|([^>]*))?>").expect("link regex should compile"));

/// Strips Slack's `mrkdwn` wire markup down to plain, human-legible text and
/// decodes the handful of HTML entities Slack escapes in `text` (`&amp;`,
/// `&lt;`, `&gt;`). Cheap and regex-based on purpose — good enough to title
/// a conversation, not a full mrkdwn renderer.
fn clean_slack_markup(text: &str) -> String {
    let cleaned = USER_MENTION_RE.replace_all(text, |caps: &regex::Captures| {
        caps.get(2).map_or_else(String::new, |name| name.as_str().to_string())
    });
    let cleaned = CHANNEL_MENTION_RE.replace_all(&cleaned, |caps: &regex::Captures| {
        caps.get(2).map_or_else(String::new, |name| name.as_str().to_string())
    });
    let cleaned = LINK_RE.replace_all(&cleaned, |caps: &regex::Captures| {
        caps.get(2).map_or_else(|| caps[1].to_string(), |label| label.as_str().to_string())
    });
    cleaned.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
}

/// Derive a fresh Slack bridge thread's `auto_title` from the raw text of
/// the inbound message that triggered its creation. `None` when the cleaned
/// text is empty (an attachment-only message, or markup that cleans down to
/// nothing) so the caller leaves `auto_title` unset and the channel-kind
/// label shows instead of a blank title.
pub fn derive_slack_channel_title(raw_text: &str) -> Option<String> {
    derive_auto_title(&clean_slack_markup(raw_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_internal_whitespace_and_newlines() {
        assert_eq!(
            derive_slack_channel_title("hello\n\n  world   foo").as_deref(),
            Some("hello world foo")
        );
    }

    #[test]
    fn truncates_long_content_on_a_char_boundary_with_ellipsis() {
        let long = "a ".repeat(60);
        let title = derive_slack_channel_title(&long).expect("long content is not empty");
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), ao_protocol::thread::MAX_TITLE_LEN + 1);
    }

    #[test]
    fn truncation_is_char_boundary_safe_for_multibyte_content() {
        let long = "😀".repeat(60);
        let title = derive_slack_channel_title(&long).expect("long content is not empty");
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), ao_protocol::thread::MAX_TITLE_LEN + 1);
    }

    #[test]
    fn drops_a_bare_user_mention() {
        assert_eq!(
            derive_slack_channel_title("<@U0123> can you help?").as_deref(),
            Some("can you help?")
        );
    }

    #[test]
    fn keeps_a_named_user_mention() {
        assert_eq!(
            derive_slack_channel_title("<@U0123|alice> can you help?").as_deref(),
            Some("alice can you help?")
        );
    }

    #[test]
    fn resolves_a_named_channel_reference() {
        assert_eq!(
            derive_slack_channel_title("ping <#C0123|general> about this").as_deref(),
            Some("ping general about this")
        );
    }

    #[test]
    fn uses_link_label_when_present() {
        assert_eq!(
            derive_slack_channel_title("see <https://example.com/doc|the doc>").as_deref(),
            Some("see the doc")
        );
    }

    #[test]
    fn uses_bare_url_when_no_label() {
        assert_eq!(
            derive_slack_channel_title("see <https://example.com/doc>").as_deref(),
            Some("see https://example.com/doc")
        );
    }

    #[test]
    fn decodes_basic_html_entities() {
        assert_eq!(
            derive_slack_channel_title("Q&amp;A: 3 &lt; 5 &gt; 1").as_deref(),
            Some("Q&A: 3 < 5 > 1")
        );
    }

    #[test]
    fn empty_text_is_none() {
        assert_eq!(derive_slack_channel_title(""), None);
    }

    #[test]
    fn markup_only_message_that_cleans_to_blank_is_none() {
        assert_eq!(derive_slack_channel_title("<@U0123>"), None);
    }
}
