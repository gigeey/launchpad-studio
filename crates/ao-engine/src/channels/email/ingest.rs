//! Formats one accepted [`FetchedEmail`] into the text block delivered to the
//! agent through [`crate::channels::submit_inbound_message`].
//!
//! The block surfaces every field the agent needs to compose a sensible
//! reply: who it's from, who else was on the thread (so the agent notices a
//! CC or forward), the subject, when it arrived, and the original
//! `Message-ID` — labeled explicitly as the value to hand back to
//! `SendEmail`'s `in_reply_to_message_id` so a reply threads correctly.

use super::imap_seam::FetchedEmail;

pub fn build_ingest_text(email: &FetchedEmail) -> String {
    let mut out = String::new();

    match &email.from_display {
        Some(name) => out.push_str(&format!("From: {name} <{}>\n", email.from_address)),
        None => out.push_str(&format!("From: {}\n", email.from_address)),
    }
    if !email.to.is_empty() {
        out.push_str(&format!("To: {}\n", email.to.join(", ")));
    }
    if !email.cc.is_empty() {
        out.push_str(&format!("Cc: {}\n", email.cc.join(", ")));
    }
    out.push_str(&format!("Subject: {}\n", email.subject.as_deref().unwrap_or("(no subject)")));
    if let Some(date) = &email.date {
        out.push_str(&format!("Date: {}\n", date.to_rfc2822()));
    }
    match &email.message_id {
        Some(id) => out.push_str(&format!(
            "Message-ID (pass this as in_reply_to_message_id when replying with SendEmail): {id}\n"
        )),
        None => out.push_str("Message-ID: (none provided by sender — reply as a new email)\n"),
    }

    out.push('\n');
    if email.body_text.trim().is_empty() {
        out.push_str("(no readable body)");
    } else {
        out.push_str(email.body_text.trim());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_email() -> FetchedEmail {
        FetchedEmail {
            uid: 1,
            from_address: "sender@example.com".to_string(),
            from_display: Some("Sender Name".to_string()),
            to: vec!["agent@example.org".to_string()],
            cc: vec![],
            subject: Some("Hello there".to_string()),
            date: chrono::DateTime::from_timestamp(1_700_000_000, 0),
            message_id: Some("abc123@example.com".to_string()),
            authentication_results: vec![],
            auto_submitted: None,
            precedence: None,
            list_unsubscribe_present: false,
            x_auto_response_suppress_present: false,
            body_text: "Hello, this is the body.".to_string(),
        }
    }

    #[test]
    fn formats_every_field_when_present() {
        let text = build_ingest_text(&base_email());
        assert!(text.contains("From: Sender Name <sender@example.com>"));
        assert!(text.contains("To: agent@example.org"));
        assert!(text.contains("Subject: Hello there"));
        assert!(text.contains("Message-ID (pass this as in_reply_to_message_id when replying with SendEmail): abc123@example.com"));
        assert!(text.contains("Hello, this is the body."));
    }

    #[test]
    fn surfaces_cc_when_present() {
        let mut email = base_email();
        email.cc = vec!["watcher@example.org".to_string()];
        let text = build_ingest_text(&email);
        assert!(text.contains("Cc: watcher@example.org"));
    }

    #[test]
    fn omits_cc_line_when_absent() {
        let text = build_ingest_text(&base_email());
        assert!(!text.contains("Cc:"));
    }

    #[test]
    fn handles_missing_display_name() {
        let mut email = base_email();
        email.from_display = None;
        let text = build_ingest_text(&email);
        assert!(text.contains("From: sender@example.com\n"));
    }

    #[test]
    fn handles_missing_subject_and_message_id_and_empty_body() {
        let mut email = base_email();
        email.subject = None;
        email.message_id = None;
        email.body_text = String::new();
        let text = build_ingest_text(&email);
        assert!(text.contains("Subject: (no subject)"));
        assert!(text.contains("Message-ID: (none provided by sender — reply as a new email)"));
        assert!(text.contains("(no readable body)"));
    }
}
