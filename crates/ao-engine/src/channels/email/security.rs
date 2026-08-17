//! Inbound sender-authorization decision for the email channel.
//!
//! Pure and side-effect free: [`evaluate_sender`] takes the handful of
//! header facts a decision actually depends on (never the raw message) and
//! returns allow or a specific [`DenyReason`]. Keeping it pure is what makes
//! it exhaustively unit-testable without a live IMAP server or a hand-built
//! MIME message.
//!
//! The one rule every check here exists to enforce: **the `From:` header is
//! attacker-controlled and must never be trusted on its own.** Anyone can put
//! any address in `From:`; the only signal that address is genuine is the
//! receiving mail server's own `Authentication-Results` verdict (SPF/DKIM/
//! DMARC), which is why authentication is checked before the `From:` address
//! is ever compared against the allow-list.

/// The header facts [`evaluate_sender`] needs. Callers (the IMAP ingest path)
/// extract these from a parsed message; this module never touches a raw
/// message or its MIME structure.
pub struct EmailMessageMeta<'a> {
    /// The bare `From:` address (`local@domain`), already extracted from any
    /// display name / angle-bracket wrapping by the caller.
    pub from_address: &'a str,
    /// Every `Authentication-Results` header value found on the message, in
    /// the order they appear in the raw headers — index 0 must be the
    /// topmost. A receiving mail server prepends its own verdict above
    /// anything already on the message, so the topmost entry is the only one
    /// this receiving deployment can vouch for; any entry an attacker forged
    /// into the message body's headers before it was relayed sorts below it
    /// and is never consulted.
    pub authentication_results: &'a [String],
    /// The `Auto-Submitted` header value, if present.
    pub auto_submitted: Option<&'a str>,
    /// The `Precedence` header value, if present.
    pub precedence: Option<&'a str>,
    /// Whether a `List-Unsubscribe` header is present (its value doesn't
    /// matter — presence alone marks bulk mail).
    pub list_unsubscribe_present: bool,
    /// Whether an `X-Auto-Response-Suppress` header is present.
    pub x_auto_response_suppress_present: bool,
}

/// Why [`evaluate_sender`] denied a message. Kept specific (rather than a
/// bare bool) so the transport can log a meaningful reason and so tests can
/// assert *which* rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// Address substring or bulk-mail header matched — see
    /// [`is_automated_or_bulk`].
    AutomatedOrBulkSender,
    /// The binding has no allowed senders configured yet. Fail-closed: an
    /// email binding with an empty allow-list accepts nothing, the same as a
    /// freshly-created Telegram binding accepts no chat until one pairs.
    AllowListEmpty,
    /// `from_address` isn't (and isn't covered by a `@domain` entry in) the
    /// binding's allow-list.
    NotOnAllowList,
    /// No `Authentication-Results` header at all, and the binding requires
    /// one (`require_auth_results: true`, the default).
    AuthenticationMissing,
    /// An `Authentication-Results` header was present but didn't establish
    /// `dmarc=pass`, or an aligned `spf=pass` / `dkim=pass`.
    AuthenticationFailed,
}

/// Decides whether an inbound message should be delivered to the agent.
///
/// Order of checks: automated/bulk filtering first (applies regardless of
/// allow-list state), then the allow-list gate, then — only when the
/// allow-list is non-empty and the sender is on it — the authentication
/// check. See the field docs on [`EmailMessageMeta`] and [`DenyReason`] for
/// what each check actually inspects.
pub fn evaluate_sender(
    meta: &EmailMessageMeta,
    allowed_senders: &[String],
    require_auth_results: bool,
) -> Result<(), DenyReason> {
    if is_automated_or_bulk(meta) {
        return Err(DenyReason::AutomatedOrBulkSender);
    }

    if allowed_senders.is_empty() {
        return Err(DenyReason::AllowListEmpty);
    }
    if !sender_on_allow_list(meta.from_address, allowed_senders) {
        return Err(DenyReason::NotOnAllowList);
    }

    match meta.authentication_results.first() {
        None => {
            if require_auth_results {
                return Err(DenyReason::AuthenticationMissing);
            }
        }
        Some(topmost) => {
            if !passes_authentication(topmost, meta.from_address) {
                return Err(DenyReason::AuthenticationFailed);
            }
        }
    }

    Ok(())
}

/// Address substrings that mark an automated sender regardless of what any
/// bulk-mail header says. Matched case-insensitively against the whole
/// address (catches both `noreply@x.com` and `bounces-noreply@x.com`).
const AUTOMATED_ADDRESS_SUBSTRINGS: &[&str] =
    &["noreply", "no-reply", "mailer-daemon", "postmaster", "bounce", "donotreply"];

/// True if `meta` looks like an automated or bulk sender: a `noreply`-shaped
/// address, or any of the standard bulk-mail signaling headers
/// (`Auto-Submitted` other than `no`, `Precedence: bulk|list|junk`,
/// `List-Unsubscribe`, `X-Auto-Response-Suppress`).
fn is_automated_or_bulk(meta: &EmailMessageMeta) -> bool {
    let addr_lower = meta.from_address.to_ascii_lowercase();
    if AUTOMATED_ADDRESS_SUBSTRINGS.iter().any(|needle| addr_lower.contains(needle)) {
        return true;
    }
    if let Some(v) = meta.auto_submitted {
        if !v.trim().eq_ignore_ascii_case("no") {
            return true;
        }
    }
    if let Some(v) = meta.precedence {
        let v = v.trim().to_ascii_lowercase();
        if v == "bulk" || v == "list" || v == "junk" {
            return true;
        }
    }
    meta.list_unsubscribe_present || meta.x_auto_response_suppress_present
}

/// Whether `from_address` is covered by `allowed_senders`: either an exact
/// case-insensitive address match, or a `@domain` entry whose domain matches
/// `from_address`'s domain case-insensitively.
fn sender_on_allow_list(from_address: &str, allowed_senders: &[String]) -> bool {
    let from_lower = from_address.to_ascii_lowercase();
    let from_domain = address_domain(from_address);
    allowed_senders.iter().any(|entry| {
        let entry_lower = entry.to_ascii_lowercase();
        if let Some(domain_entry) = entry_lower.strip_prefix('@') {
            from_domain.as_deref() == Some(domain_entry)
        } else {
            entry_lower == from_lower
        }
    })
}

/// Lowercased domain portion of a bare `local@domain` address, or `None` if
/// there's no `@`.
fn address_domain(address: &str) -> Option<String> {
    let (_, domain) = address.rsplit_once('@')?;
    Some(domain.trim().to_ascii_lowercase())
}

/// Evaluates the topmost `Authentication-Results` header value against
/// `from_address`'s domain. Accepts iff `dmarc=pass`, or `spf=pass` with an
/// aligned `smtp.mailfrom`/`smtp.helo` domain, or `dkim=pass` with an aligned
/// `header.d` domain. `dmarc=pass` needs no separate alignment check — DMARC
/// itself only passes when its own underlying SPF or DKIM check was already
/// aligned, so a verifying server only ever stamps `dmarc=pass` after
/// confirming that.
fn passes_authentication(topmost_auth_results: &str, from_address: &str) -> bool {
    let Some(from_domain) = address_domain(from_address) else {
        return false;
    };
    let clauses: Vec<&str> = topmost_auth_results.split(';').map(str::trim).collect();

    if clauses.iter().any(|c| clause_result(c, "dmarc").as_deref() == Some("pass")) {
        return true;
    }

    for clause in &clauses {
        if clause_result(clause, "spf").as_deref() != Some("pass") {
            continue;
        }
        let domain = clause_param_domain(clause, "smtp.mailfrom")
            .or_else(|| clause_param_domain(clause, "smtp.helo"));
        if domain.is_some_and(|d| domains_aligned(&d, &from_domain)) {
            return true;
        }
    }

    for clause in &clauses {
        if clause_result(clause, "dkim").as_deref() != Some("pass") {
            continue;
        }
        if let Some(domain) = clause_param_domain(clause, "header.d") {
            if domains_aligned(&domain, &from_domain) {
                return true;
            }
        }
    }

    false
}

/// If `clause` (one `;`-delimited segment of an `Authentication-Results`
/// value) is a `<mechanism>=<result>` clause for `mechanism`, returns the
/// lowercased result word. `None` if this clause is for a different
/// mechanism (or isn't a mechanism clause at all, e.g. the leading
/// `authserv-id` segment).
fn clause_result(clause: &str, mechanism: &str) -> Option<String> {
    let clause = clause.trim();
    let prefix = format!("{mechanism}=");
    let rest = clause.get(..prefix.len())?.eq_ignore_ascii_case(&prefix).then(|| &clause[prefix.len()..])?;
    let result = rest.split_whitespace().next()?;
    Some(result.to_ascii_lowercase())
}

/// Finds `<param>=<value>` within `clause` (e.g. `smtp.mailfrom=` or
/// `header.d=`) and returns the domain it names: the part after `@` when the
/// value is a full address, or the value verbatim when it's already a bare
/// domain. `None` if `param` doesn't appear in this clause.
fn clause_param_domain(clause: &str, param: &str) -> Option<String> {
    let lower = clause.to_ascii_lowercase();
    let needle = format!("{}=", param.to_ascii_lowercase());
    let start = lower.find(&needle)? + needle.len();
    let token: String = clause[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ')' && *c != ';')
        .collect();
    if token.is_empty() {
        return None;
    }
    let domain = token.rsplit('@').next().unwrap_or(&token);
    let domain = domain.trim_matches(|c: char| c == '"' || c == '<' || c == '>');
    if domain.is_empty() {
        None
    } else {
        Some(domain.to_ascii_lowercase())
    }
}

/// Approximate DMARC "relaxed" alignment: exact match, or either domain is a
/// subdomain of the other (`mail.example.com` aligns with `example.com`).
fn domains_aligned(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('.').to_ascii_lowercase();
    let b = b.trim_end_matches('.').to_ascii_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta<'a>(from_address: &'a str, auth_results: &'a [String]) -> EmailMessageMeta<'a> {
        EmailMessageMeta {
            from_address,
            authentication_results: auth_results,
            auto_submitted: None,
            precedence: None,
            list_unsubscribe_present: false,
            x_auto_response_suppress_present: false,
        }
    }

    fn allow_list(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    // --- Authenticated allowed sender -> allow ---

    #[test]
    fn dmarc_pass_allowed_sender_is_allowed() {
        let auth = vec!["mx.example.net; dmarc=pass (p=REJECT) header.from=example.com".to_string()];
        let m = meta("user@example.com", &auth);
        assert_eq!(evaluate_sender(&m, &allow_list(&["user@example.com"]), true), Ok(()));
    }

    #[test]
    fn spf_pass_with_aligned_mailfrom_is_allowed() {
        let auth = vec![
            "mx.example.net; spf=pass (example.net: domain of user@example.com designates 1.2.3.4 as permitted sender) smtp.mailfrom=user@example.com".to_string(),
        ];
        let m = meta("user@example.com", &auth);
        assert_eq!(evaluate_sender(&m, &allow_list(&["user@example.com"]), true), Ok(()));
    }

    #[test]
    fn dkim_pass_with_aligned_header_d_is_allowed() {
        let auth = vec![
            "mx.example.net; dkim=pass header.i=@example.com header.s=sel header.d=example.com header.b=abc".to_string(),
        ];
        let m = meta("user@example.com", &auth);
        assert_eq!(evaluate_sender(&m, &allow_list(&["user@example.com"]), true), Ok(()));
    }

    #[test]
    fn subdomain_alignment_is_accepted_for_dkim() {
        let auth = vec!["mx.example.net; dkim=pass header.d=example.com".to_string()];
        let m = meta("user@mail.example.com", &auth);
        assert_eq!(evaluate_sender(&m, &allow_list(&["user@mail.example.com"]), true), Ok(()));
    }

    // --- Spoofed From with failing/mismatched auth -> deny ---

    #[test]
    fn spoofed_from_with_failing_auth_is_denied() {
        let auth = vec!["mx.example.net; dkim=fail header.d=example.com; spf=fail smtp.mailfrom=user@example.com; dmarc=fail".to_string()];
        let m = meta("user@example.com", &auth);
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AuthenticationFailed)
        );
    }

    #[test]
    fn spf_pass_but_mismatched_domain_is_denied() {
        // spf passes for attacker.com, not for the domain in From:.
        let auth = vec!["mx.example.net; spf=pass smtp.mailfrom=user@attacker.com".to_string()];
        let m = meta("user@example.com", &auth);
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AuthenticationFailed)
        );
    }

    #[test]
    fn dkim_pass_but_mismatched_domain_is_denied() {
        let auth = vec!["mx.example.net; dkim=pass header.d=attacker.com".to_string()];
        let m = meta("user@example.com", &auth);
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AuthenticationFailed)
        );
    }

    // --- Missing Authentication-Results ---

    #[test]
    fn missing_auth_results_denied_when_required() {
        let m = meta("user@example.com", &[]);
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AuthenticationMissing)
        );
    }

    #[test]
    fn missing_auth_results_allowed_when_not_required() {
        let m = meta("user@example.com", &[]);
        assert_eq!(evaluate_sender(&m, &allow_list(&["user@example.com"]), false), Ok(()));
    }

    // --- Automated / bulk senders -> deny ---

    #[test]
    fn noreply_address_is_denied_even_if_authenticated_and_allow_listed() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let m = meta("noreply@example.com", &auth);
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["noreply@example.com"]), true),
            Err(DenyReason::AutomatedOrBulkSender)
        );
    }

    #[test]
    fn bulk_precedence_header_is_denied() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let mut m = meta("user@example.com", &auth);
        m.precedence = Some("bulk");
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AutomatedOrBulkSender)
        );
    }

    #[test]
    fn auto_submitted_non_no_is_denied() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let mut m = meta("user@example.com", &auth);
        m.auto_submitted = Some("auto-replied");
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AutomatedOrBulkSender)
        );
    }

    #[test]
    fn auto_submitted_no_is_not_bulk() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let mut m = meta("user@example.com", &auth);
        m.auto_submitted = Some("no");
        assert_eq!(evaluate_sender(&m, &allow_list(&["user@example.com"]), true), Ok(()));
    }

    #[test]
    fn list_unsubscribe_present_is_denied() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let mut m = meta("user@example.com", &auth);
        m.list_unsubscribe_present = true;
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AutomatedOrBulkSender)
        );
    }

    #[test]
    fn x_auto_response_suppress_present_is_denied() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let mut m = meta("user@example.com", &auth);
        m.x_auto_response_suppress_present = true;
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AutomatedOrBulkSender)
        );
    }

    // --- Allow-list membership ---

    #[test]
    fn sender_not_on_allow_list_is_denied() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let m = meta("stranger@example.com", &auth);
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::NotOnAllowList)
        );
    }

    #[test]
    fn empty_allow_list_denies_everyone() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let m = meta("user@example.com", &auth);
        assert_eq!(evaluate_sender(&m, &allow_list(&[]), true), Err(DenyReason::AllowListEmpty));
    }

    #[test]
    fn domain_allow_list_entry_matches_any_sender_on_that_domain() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let m = meta("anyone@example.com", &auth);
        assert_eq!(evaluate_sender(&m, &allow_list(&["@example.com"]), true), Ok(()));
    }

    #[test]
    fn allow_list_match_is_case_insensitive() {
        let auth = vec!["mx.example.net; dmarc=pass".to_string()];
        let m = meta("User@Example.com", &auth);
        assert_eq!(evaluate_sender(&m, &allow_list(&["user@example.com"]), true), Ok(()));
    }

    // --- Injected second Authentication-Results header doesn't bypass ---

    #[test]
    fn injected_second_auth_results_header_does_not_bypass_a_failing_topmost() {
        // Topmost (the receiving server's own stamp) fails; a forged second
        // copy claiming dmarc=pass sits below it, exactly as it would if an
        // attacker pre-pended a fake header before the message was relayed.
        let auth = vec![
            "mx.example.net; dmarc=fail; spf=fail smtp.mailfrom=user@attacker.com".to_string(),
            "forged.invalid; dmarc=pass".to_string(),
        ];
        let m = meta("user@example.com", &auth);
        assert_eq!(
            evaluate_sender(&m, &allow_list(&["user@example.com"]), true),
            Err(DenyReason::AuthenticationFailed)
        );
    }

}
