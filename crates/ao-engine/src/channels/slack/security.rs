//! Inbound authorization decision for the Slack channel: whether a
//! message's channel and author admit it to the agent's binding.
//!
//! Pure and side-effect free, mirroring
//! [`crate::channels::discord::security::is_allowed`]'s OR-across-lists
//! shape, but with only two facts to weigh instead of four — Slack has no
//! guild/role concept, just a channel id (`C.../D.../G...`) and a user id
//! (`U...`). [`is_allowed`] takes those two ids plus the binding's two
//! allow-lists directly; resolving a raw Socket Mode event down to them is
//! [`super::runner`]'s job, not this module's.

/// Whether an inbound Slack message should be delivered to the agent.
/// OR semantics: a `channel_id` match against `allowed_channels`, or a
/// `user_id` match against `allowed_users`, is sufficient on its own —
/// neither list needs the other to also match. A literal `"*"` entry in
/// either list opens that dimension to everyone, the same wildcard
/// convention as [`crate::channels::discord::security::is_allowed`]'s
/// `allowed_users`.
///
/// Both lists empty is a deliberate fail-closed reject, checked up front
/// rather than left as an incidental consequence of two empty `.any()`
/// calls: an unconfigured binding must accept nothing, the same posture as
/// [`crate::channels::discord::security::is_allowed`]'s
/// `allowed_users`/`allowed_roles` gate and Telegram's own allow-list. The
/// wildcard only ever widens a non-empty list — it can't be used to smuggle
/// past the empty-list reject, since an empty list has no entries to hold
/// one.
pub fn is_allowed(channel_id: &str, user_id: &str, allowed_channels: &[String], allowed_users: &[String]) -> bool {
    if allowed_channels.is_empty() && allowed_users.is_empty() {
        return false;
    }
    allowed_channels.iter().any(|c| c == "*" || c == channel_id) || allowed_users.iter().any(|u| u == "*" || u == user_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_both_allow_lists_reject_everyone() {
        assert!(!is_allowed("C123", "U456", &[], &[]));
    }

    #[test]
    fn channel_match_alone_is_sufficient() {
        assert!(is_allowed("C123", "U456", &strs(&["C123"]), &[]));
    }

    #[test]
    fn user_match_alone_is_sufficient() {
        assert!(is_allowed("C123", "U456", &[], &strs(&["U456"])));
    }

    #[test]
    fn neither_channel_nor_user_listed_is_denied() {
        assert!(!is_allowed("C123", "U456", &strs(&["C999"]), &strs(&["U999"])));
    }

    #[test]
    fn or_semantics_a_user_match_admits_even_when_the_channel_list_misses() {
        assert!(is_allowed("C123", "U456", &strs(&["C999"]), &strs(&["U456"])));
    }

    #[test]
    fn or_semantics_a_channel_match_admits_even_when_the_user_list_misses() {
        assert!(is_allowed("C123", "U456", &strs(&["C123"]), &strs(&["U999"])));
    }

    #[test]
    fn a_configured_but_non_matching_channel_list_still_fails_closed_with_no_user_list() {
        assert!(!is_allowed("C123", "U456", &strs(&["C999"]), &[]));
    }

    #[test]
    fn a_configured_but_non_matching_user_list_still_fails_closed_with_no_channel_list() {
        assert!(!is_allowed("C123", "U456", &[], &strs(&["U999"])));
    }

    #[test]
    fn wildcard_user_entry_admits_an_arbitrary_user() {
        assert!(is_allowed("C0BJYGZ5WV8", "UGPEWJC4A", &[], &strs(&["*"])));
    }

    #[test]
    fn wildcard_channel_entry_admits_an_arbitrary_channel() {
        assert!(is_allowed("C0BJYGZ5WV8", "UGPEWJC4A", &strs(&["*"]), &[]));
    }

    #[test]
    fn a_specific_non_matching_entry_is_still_rejected_when_no_wildcard_is_present() {
        assert!(!is_allowed("C0BJYGZ5WV8", "UGPEWJC4A", &strs(&["C999"]), &strs(&["U999"])));
    }
}
