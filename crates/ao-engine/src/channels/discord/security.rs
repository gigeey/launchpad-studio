//! Inbound authorization decision for the Discord channel, plus the related
//! decision of whether a binding needs the privileged `GUILD_MEMBERS`
//! intent to enforce its own config.
//!
//! Pure and side-effect free, mirroring
//! [`crate::channels::email::security::evaluate_sender`]: [`is_allowed`]
//! takes the handful of facts a decision actually depends on (never a raw
//! Gateway payload) and returns a plain `bool`. Any role/username
//! resolution that needs a network call (e.g. fetching a DM author's roles
//! in `dm_role_auth_guild`) happens in [`super::runner`] before this module
//! is ever consulted — by the time `is_allowed` runs, `member_roles` is
//! already whatever was resolvable.

/// The facts [`is_allowed`] needs about one inbound message's author and
/// context. Callers ([`super::runner`]) build this from a parsed
/// `MESSAGE_CREATE` plus whatever role resolution it already performed.
pub struct AuthContext<'a> {
    pub author_id: &'a str,
    pub author_username: &'a str,
    /// `true` for a DM, `false` for a guild-channel message.
    pub is_dm: bool,
    pub channel_id: &'a str,
    /// Roles the author holds in the relevant guild — the message's own
    /// guild for a guild message, or `dm_role_auth_guild`'s guild for a DM
    /// when [`Self::role_auth_enabled`] is set. Empty when role
    /// resolution wasn't performed or doesn't apply.
    pub member_roles: &'a [String],
    /// Whether role-based authorization applies to this message at all.
    /// Always `true` for a guild message. For a DM, only `true` when the
    /// binding set `dm_role_auth_guild` — see [`role_auth_enabled`].
    pub role_auth_enabled: bool,
    /// Whether `channel_id` is itself a Discord thread. A thread carries its
    /// own channel id, distinct from the channel it lives under, so
    /// `allowed_channels` entries naming the parent never match `channel_id`
    /// directly — see [`is_allowed`]'s doc for how `parent_channel_id`
    /// covers that.
    pub is_thread: bool,
    /// The thread's parent channel id, when [`Self::is_thread`] is true and
    /// the parent was resolvable. Callers resolve this fact (e.g. via
    /// `super::channel_meta::resolve_channel_meta`) before calling
    /// [`is_allowed`] — this module never performs I/O itself.
    pub parent_channel_id: Option<&'a str>,
}

/// Whether role-based authorization applies to a message from this context.
/// Always enabled for a guild message; for a DM, only when the binding
/// explicitly opted in via `dm_role_auth_guild`. A DM has no guild of its
/// own to resolve roles against, so leaving this disabled by default
/// prevents a role grant in *some* guild the bot happens to share with the
/// user from silently authorizing them in every DM — the binding must name
/// the one guild whose roles it trusts for that.
pub fn role_auth_enabled(is_dm: bool, dm_role_auth_guild: Option<&str>) -> bool {
    !is_dm || dm_role_auth_guild.is_some()
}

/// Decides whether an inbound Discord message should be delivered to the
/// agent. OR semantics across `allowed_users` and `allowed_roles`: either
/// list matching is sufficient. Both empty fails closed — matching
/// [`crate::channels::email::security::evaluate_sender`]'s
/// `AllowListEmpty` behavior, an unconfigured binding accepts nothing.
///
/// Order of checks: fail-closed gate first, then the (guild-only) channel
/// allow-list, then user-or-role membership.
///
/// The channel allow-list check accepts a direct `channel_id` match, or —
/// when `ctx.is_thread` is set — a match against `ctx.parent_channel_id`
/// instead. Without that fallback, allow-listing a channel would silently
/// reject every message posted in a thread under it, since a thread's
/// `channel_id` is never the same as its parent's.
pub fn is_allowed(
    ctx: &AuthContext,
    allowed_users: &[String],
    allowed_roles: &[String],
    allowed_channels: &[String],
) -> bool {
    if allowed_users.is_empty() && allowed_roles.is_empty() {
        return false;
    }
    if !ctx.is_dm && !allowed_channels.is_empty() && !channel_admitted(ctx, allowed_channels) {
        return false;
    }
    user_matches(ctx, allowed_users) || role_matches(ctx, allowed_roles)
}

fn channel_admitted(ctx: &AuthContext, allowed_channels: &[String]) -> bool {
    if allowed_channels.iter().any(|c| c == ctx.channel_id) {
        return true;
    }
    ctx.is_thread && ctx.parent_channel_id.is_some_and(|parent| allowed_channels.iter().any(|c| c == parent))
}

/// `"*"` is an open-mode wildcard entry: any user matches. Otherwise an
/// entry matches by exact user id or by case-insensitive username.
fn user_matches(ctx: &AuthContext, allowed_users: &[String]) -> bool {
    allowed_users
        .iter()
        .any(|entry| entry == "*" || entry == ctx.author_id || entry.eq_ignore_ascii_case(ctx.author_username))
}

fn role_matches(ctx: &AuthContext, allowed_roles: &[String]) -> bool {
    ctx.role_auth_enabled && !allowed_roles.is_empty() && ctx.member_roles.iter().any(|r| allowed_roles.contains(r))
}

/// Whether a binding needs the privileged `GUILD_MEMBERS` intent to enforce
/// its own `allowed_users`/`allowed_roles` config: any configured role
/// (`allowed_roles` non-empty), or an `allowed_users` entry that names a
/// username rather than a numeric snowflake id. A bare id needs no
/// resolution (it's compared directly against the message author's id), and
/// the `"*"` wildcard needs none either — it matches everyone without
/// looking anyone up.
pub fn needs_members_intent(allowed_users: &[String], allowed_roles: &[String]) -> bool {
    if !allowed_roles.is_empty() {
        return true;
    }
    allowed_users.iter().any(|entry| entry != "*" && !is_numeric_snowflake(entry))
}

fn is_numeric_snowflake(entry: &str) -> bool {
    !entry.is_empty() && entry.chars().all(|c| c.is_ascii_digit())
}

/// Whether an inbound message's author should be ignored outright, before
/// any authorization check runs: the bot's own messages (compared by id
/// against the `own_user_id` captured from `READY`) and every other bot's
/// messages.
pub fn should_ignore_author(author_id: &str, author_is_bot: bool, own_user_id: &str) -> bool {
    author_is_bot || author_id == own_user_id
}

/// The facts [`is_bot_mentioned`] needs about one inbound message's mention
/// data. Callers ([`super::runner`]) build this from a parsed
/// `MESSAGE_CREATE`'s `mentions`/`content` fields plus the bot's own user id
/// captured from `READY`.
pub struct MentionContext<'a> {
    pub own_user_id: Option<&'a str>,
    pub mentioned_user_ids: &'a [String],
    pub content: &'a str,
}

/// Whether the bot itself was explicitly @-mentioned in a message: its id
/// appears in the parsed `mentions` array, or — as a fallback for payloads
/// where that array is absent — the raw content contains a literal
/// `<@id>` or `<@!id>` (nickname-mention form) mention tag.
///
/// `@everyone`/`@here` deliberately never count, with no special-casing
/// needed to make that true: Discord never adds the bot to `mentions` for
/// one, and the raw content carries the literal text `@everyone`/`@here`
/// rather than a `<@id>` tag, so neither check above can ever match it.
/// Treating a broadcast like that as a mention of the bot specifically is
/// the single most common way a channel bot ends up muted by an admin for
/// spamming replies to messages nobody addressed to it.
pub fn is_bot_mentioned(ctx: &MentionContext) -> bool {
    let Some(own_id) = ctx.own_user_id else {
        return false;
    };
    if ctx.mentioned_user_ids.iter().any(|id| id == own_id) {
        return true;
    }
    ctx.content.contains(&format!("<@{own_id}>")) || ctx.content.contains(&format!("<@!{own_id}>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(author_id: &'a str, author_username: &'a str, is_dm: bool, channel_id: &'a str, member_roles: &'a [String], role_auth_enabled: bool) -> AuthContext<'a> {
        ctx_in_channel(author_id, author_username, is_dm, channel_id, member_roles, role_auth_enabled, false, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn ctx_in_channel<'a>(
        author_id: &'a str,
        author_username: &'a str,
        is_dm: bool,
        channel_id: &'a str,
        member_roles: &'a [String],
        role_auth_enabled: bool,
        is_thread: bool,
        parent_channel_id: Option<&'a str>,
    ) -> AuthContext<'a> {
        AuthContext {
            author_id,
            author_username,
            is_dm,
            channel_id,
            member_roles,
            role_auth_enabled,
            is_thread,
            parent_channel_id,
        }
    }

    fn strs(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    // --- Fail-closed / wildcard / OR semantics ---

    #[test]
    fn empty_allow_lists_reject_everyone() {
        let c = ctx("1", "alice", false, "chan", &[], true);
        assert!(!is_allowed(&c, &[], &[], &[]));
    }

    #[test]
    fn wildcard_user_entry_opens_to_everyone() {
        let c = ctx("999999", "whoever", false, "chan", &[], true);
        assert!(is_allowed(&c, &strs(&["*"]), &[], &[]));
    }

    #[test]
    fn user_id_match_alone_is_sufficient() {
        let c = ctx("42", "alice", false, "chan", &[], true);
        assert!(is_allowed(&c, &strs(&["42"]), &[], &[]));
    }

    #[test]
    fn username_match_is_case_insensitive() {
        let c = ctx("42", "Alice", false, "chan", &[], true);
        assert!(is_allowed(&c, &strs(&["alice"]), &[], &[]));
    }

    #[test]
    fn user_not_on_either_list_is_denied() {
        let roles = strs(&["role-a"]);
        let c = ctx("42", "alice", false, "chan", &roles, true);
        assert!(!is_allowed(&c, &strs(&["someone-else"]), &strs(&["role-b"]), &[]));
    }

    #[test]
    fn role_match_alone_is_sufficient_even_with_no_user_match() {
        let roles = strs(&["role-a"]);
        let c = ctx("42", "alice", false, "chan", &roles, true);
        assert!(is_allowed(&c, &strs(&["nobody"]), &strs(&["role-a"]), &[]));
    }

    #[test]
    fn or_semantics_either_user_or_role_match_admits() {
        let by_user = ctx("42", "alice", false, "chan", &[], true);
        assert!(is_allowed(&by_user, &strs(&["42"]), &strs(&["role-z"]), &[]));

        let roles = strs(&["role-z"]);
        let by_role = ctx("999", "stranger", false, "chan", &roles, true);
        assert!(is_allowed(&by_role, &strs(&["42"]), &strs(&["role-z"]), &[]));
    }

    // --- Guild-scoped role checks ---

    #[test]
    fn guild_message_role_match_admits() {
        let roles = strs(&["admins"]);
        let c = ctx("42", "alice", false, "chan", &roles, true);
        assert!(is_allowed(&c, &[], &strs(&["admins"]), &[]));
    }

    #[test]
    fn guild_message_role_mismatch_denies() {
        let roles = strs(&["members"]);
        let c = ctx("42", "alice", false, "chan", &roles, true);
        assert!(!is_allowed(&c, &[], &strs(&["admins"]), &[]));
    }

    // --- DM role-auth gating ---

    #[test]
    fn dm_role_auth_disabled_by_default_even_with_a_matching_role() {
        // role_auth_enabled = false, as computed for a DM with no
        // dm_role_auth_guild configured — the role list carried in `ctx`
        // must not be consulted at all.
        let roles = strs(&["admins"]);
        let c = ctx("42", "alice", true, "dm-chan", &roles, false);
        assert!(!is_allowed(&c, &[], &strs(&["admins"]), &[]), "DM role auth must stay disabled without dm_role_auth_guild");
    }

    #[test]
    fn dm_role_auth_enabled_once_dm_role_auth_guild_is_set() {
        let roles = strs(&["admins"]);
        let c = ctx("42", "alice", true, "dm-chan", &roles, true);
        assert!(is_allowed(&c, &[], &strs(&["admins"]), &[]));
    }

    #[test]
    fn role_auth_enabled_helper_matches_the_documented_rule() {
        assert!(role_auth_enabled(false, None), "guild messages always have role auth enabled");
        assert!(role_auth_enabled(false, Some("guild-1")));
        assert!(!role_auth_enabled(true, None), "DM without dm_role_auth_guild must disable role auth");
        assert!(role_auth_enabled(true, Some("guild-1")), "DM with dm_role_auth_guild must enable role auth");
    }

    // --- Channel allow-list (guild messages only) ---

    #[test]
    fn allowed_channels_gates_guild_messages() {
        let c = ctx("42", "alice", false, "chan-b", &[], true);
        assert!(!is_allowed(&c, &strs(&["42"]), &[], &strs(&["chan-a"])));
    }

    #[test]
    fn allowed_channels_admits_a_listed_channel() {
        let c = ctx("42", "alice", false, "chan-a", &[], true);
        assert!(is_allowed(&c, &strs(&["42"]), &[], &strs(&["chan-a"])));
    }

    #[test]
    fn empty_allowed_channels_means_every_channel_is_eligible() {
        let c = ctx("42", "alice", false, "any-chan", &[], true);
        assert!(is_allowed(&c, &strs(&["42"]), &[], &[]));
    }

    #[test]
    fn allowed_channels_does_not_gate_dms() {
        // A DM has no guild channel to check against a guild's allow-list;
        // the channel gate only ever applies to guild messages.
        let roles = strs(&["42"]);
        let c = ctx("42", "alice", true, "dm-chan", &roles, true);
        assert!(is_allowed(&c, &strs(&["42"]), &[], &strs(&["some-other-channel"])));
    }

    // --- Thread allow-list fallback to parent ---

    #[test]
    fn thread_inside_an_allowed_parent_is_permitted() {
        // The thread's own channel id ("thread-99") is not itself on the
        // allow-list, only its parent ("chan-a") is — this is the exact
        // shape a real allow-listed parent channel produces.
        let c = ctx_in_channel("42", "alice", false, "thread-99", &[], true, true, Some("chan-a"));
        assert!(is_allowed(&c, &strs(&["42"]), &[], &strs(&["chan-a"])));
    }

    #[test]
    fn thread_inside_a_non_allowed_parent_is_still_rejected() {
        let c = ctx_in_channel("42", "alice", false, "thread-99", &[], true, true, Some("chan-x"));
        assert!(!is_allowed(&c, &strs(&["42"]), &[], &strs(&["chan-a"])));
    }

    #[test]
    fn thread_with_an_unresolved_parent_falls_back_to_a_direct_id_match_only() {
        // `parent_channel_id: None` is what an unresolved channel-meta
        // lookup produces — must fail closed rather than admit everything.
        let c = ctx_in_channel("42", "alice", false, "thread-99", &[], true, true, None);
        assert!(!is_allowed(&c, &strs(&["42"]), &[], &strs(&["chan-a"])));
    }

    #[test]
    fn empty_allowed_channels_admits_a_thread_too() {
        // Unchanged behavior: an empty allow-list means every channel
        // (thread or not) is eligible, so the parent fallback never even
        // needs to be consulted.
        let c = ctx_in_channel("42", "alice", false, "thread-99", &[], true, true, Some("chan-a"));
        assert!(is_allowed(&c, &strs(&["42"]), &[], &[]));
    }

    #[test]
    fn a_direct_thread_channel_id_match_still_works_without_needing_the_parent_fallback() {
        // Unchanged behavior: if a caller ever does list a thread's own id
        // directly, that still matches on its own.
        let c = ctx_in_channel("42", "alice", false, "thread-99", &[], true, true, Some("chan-a"));
        assert!(is_allowed(&c, &strs(&["42"]), &[], &strs(&["thread-99"])));
    }

    #[test]
    fn non_thread_channel_behavior_is_unaffected_by_the_parent_fallback() {
        // Unchanged behavior: for an ordinary (non-thread) channel, only a
        // direct id match ever admits it, exactly as before this fix.
        let c = ctx_in_channel("42", "alice", false, "chan-b", &[], true, false, Some("chan-a"));
        assert!(
            !is_allowed(&c, &strs(&["42"]), &[], &strs(&["chan-a"])),
            "a non-thread channel must never be admitted via a 'parent' id — is_thread gates the fallback"
        );
    }

    // --- Members-intent selection ---

    #[test]
    fn no_intent_needed_for_numeric_ids_or_wildcard() {
        assert!(!needs_members_intent(&strs(&["123456789012345678", "*"]), &[]));
    }

    #[test]
    fn intent_needed_for_a_username_entry() {
        assert!(needs_members_intent(&strs(&["some_username"]), &[]));
    }

    #[test]
    fn intent_needed_whenever_any_role_is_configured() {
        assert!(needs_members_intent(&[], &strs(&["role-a"])));
    }

    #[test]
    fn wildcard_alone_never_triggers_the_privileged_intent() {
        assert!(!needs_members_intent(&strs(&["*"]), &[]));
    }

    #[test]
    fn no_intent_needed_when_both_lists_are_empty() {
        assert!(!needs_members_intent(&[], &[]));
    }

    // --- Self / other-bot filtering ---

    #[test]
    fn own_message_is_ignored() {
        assert!(should_ignore_author("777", false, "777"));
    }

    #[test]
    fn other_bots_are_ignored() {
        assert!(should_ignore_author("111", true, "777"));
    }

    #[test]
    fn a_human_authored_message_from_someone_else_is_not_ignored() {
        assert!(!should_ignore_author("111", false, "777"));
    }

    // --- Mention detection ---

    fn mention_ctx<'a>(own_user_id: Option<&'a str>, mentioned_user_ids: &'a [String], content: &'a str) -> MentionContext<'a> {
        MentionContext { own_user_id, mentioned_user_ids, content }
    }

    #[test]
    fn mentioned_via_the_mentions_array_is_detected() {
        let ids = strs(&["777"]);
        let ctx = mention_ctx(Some("777"), &ids, "hey there");
        assert!(is_bot_mentioned(&ctx));
    }

    #[test]
    fn mentioned_via_content_tag_is_detected_as_a_fallback() {
        let ctx = mention_ctx(Some("777"), &[], "hello <@777> how are you");
        assert!(is_bot_mentioned(&ctx));
    }

    #[test]
    fn mentioned_via_nickname_content_tag_is_detected_as_a_fallback() {
        let ctx = mention_ctx(Some("777"), &[], "hello <@!777> how are you");
        assert!(is_bot_mentioned(&ctx));
    }

    #[test]
    fn not_mentioned_when_neither_array_nor_content_names_the_bot() {
        let ids = strs(&["111"]);
        let ctx = mention_ctx(Some("777"), &ids, "hello world");
        assert!(!is_bot_mentioned(&ctx));
    }

    #[test]
    fn mention_everyone_alone_does_not_count_as_a_bot_mention() {
        // No `mentions` entry and no `<@id>`/`<@!id>` tag in the content —
        // exactly what a real `@everyone` message's payload looks like.
        let ctx = mention_ctx(Some("777"), &[], "@everyone please look at this");
        assert!(!is_bot_mentioned(&ctx), "@everyone/@here must never wake the bot");
    }

    #[test]
    fn own_user_id_none_returns_false_regardless_of_content() {
        let ids = strs(&["777"]);
        let ctx = mention_ctx(None, &ids, "<@777>");
        assert!(!is_bot_mentioned(&ctx));
    }

    // --- Authorization stays independent of, and precedes, the mention gate ---

    #[test]
    fn is_allowed_denies_an_unauthorized_sender_even_when_the_bot_is_mentioned() {
        // `is_allowed` and `is_bot_mentioned` are deliberately independent
        // pure functions — neither's input depends on the other's result —
        // mirroring the ordering `super::runner::handle_message_create_inner`
        // enforces: authorization runs first and fail-closed, and a mention
        // (or the engagement gate built on top of it) must never widen who
        // gets through.
        let content = "hey <@777> can you help with this";
        let mention = mention_ctx(Some("777"), &[], content);
        assert!(is_bot_mentioned(&mention), "sanity: this message does mention the bot");

        let auth = ctx("999", "stranger", false, "chan", &[], true);
        assert!(
            !is_allowed(&auth, &strs(&["42"]), &[], &[]),
            "an unauthorized sender must still be denied even though the message mentions the bot"
        );
    }
}
