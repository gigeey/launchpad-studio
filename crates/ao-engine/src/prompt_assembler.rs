//! Compose a coordinator-family system prompt from an ordered list of section
//! ids and an optional identity override.
//!
//! Sections are looked up via [`crate::prompt_sections::lookup`]. Unknown ids
//! produce an [`AssembleError::UnknownSection`] error rather than being
//! silently skipped — a typo'd profile composition should fail loudly.

use crate::prompt_sections::{lookup, COPILOT_IDENTITY_OVERRIDE, COPILOT_SECTION_IDS};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AssembleError {
    #[error("unknown prompt section id: `{0}`")]
    UnknownSection(String),
}

/// Compose `section_ids` (in order) into a single system prompt string.
///
/// When `identity_override` is `Some(text)` AND the section list contains
/// the `identity` section, the body of that section is replaced verbatim
/// with `text`. The override should include its own `## ` heading — the
/// assembler does not synthesize one.
///
/// Sections are joined by a blank line. The resulting string still contains
/// any `{{...}}` placeholders the section files declare; substituting those
/// is the caller's responsibility (the values come from runtime state like
/// the team profile and member roster).
pub fn assemble_prompt(
    section_ids: &[&str],
    identity_override: Option<&str>,
) -> Result<String, AssembleError> {
    let mut bodies: Vec<&str> = Vec::with_capacity(section_ids.len());
    for id in section_ids {
        let section =
            lookup(id).ok_or_else(|| AssembleError::UnknownSection((*id).to_string()))?;
        let body: &str = if *id == "identity" {
            identity_override.unwrap_or(section.body)
        } else {
            section.body
        };
        bodies.push(body.trim_end());
    }
    Ok(bodies.join("\n\n"))
}

/// Assemble the seeded `tasklist-copilot` profile prompt.
///
/// Composes [`COPILOT_SECTION_IDS`] with the [`COPILOT_IDENTITY_OVERRIDE`]
/// applied to the `identity` slot. Pure (no mutable state, no I/O) — the
/// "registration" of the profile is the const data this function reads,
/// so calling it repeatedly never duplicates anything.
pub fn assemble_copilot_prompt() -> Result<String, AssembleError> {
    assemble_prompt(COPILOT_SECTION_IDS, Some(COPILOT_IDENTITY_OVERRIDE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_sections::ALL_SECTIONS;

    /// The canonical coordinator section list — the same ids and order the
    /// `prompt_sections` module exposes via `ALL_SECTIONS`.
    fn coordinator_section_ids() -> Vec<&'static str> {
        ALL_SECTIONS.iter().map(|s| s.id).collect()
    }

    #[test]
    fn full_coordinator_assembly_includes_every_section_anchor() {
        let ids = coordinator_section_ids();
        let assembled = assemble_prompt(&ids, None).expect("assembly should succeed");

        // Every section's `## ` heading appears.
        assert!(assembled.contains("## Team Coordination Role"));
        assert!(assembled.contains("## Task Lifecycle"));
        assert!(assembled.contains("## Task Notification Format"));
        assert!(assembled.contains("## Delegation Format"));
        assert!(assembled.contains("## Tasklist Format"));
        assert!(assembled.contains("## Round Limit"));
        assert!(assembled.contains("## Tools Reference"));
        assert!(assembled.contains("## Conversation Style"));
        assert!(assembled.contains("## Conversation Context"));

        // Section ordering matches the supplied id ordering.
        let identity_pos = assembled.find("## Team Coordination Role").unwrap();
        let lifecycle_pos = assembled.find("## Task Lifecycle").unwrap();
        let routing_pos = assembled.find("## Delegation Format").unwrap();
        assert!(
            identity_pos < lifecycle_pos && lifecycle_pos < routing_pos,
            "sections should appear in the order they were requested",
        );

        // Sections are separated by a blank line (no triple-newline
        // collisions even though the source files end with `\n`).
        assert!(!assembled.contains("\n\n\n"));
    }

    #[test]
    fn partial_assembly_excludes_omitted_section() {
        // Drop `routing_and_dispatch` — the co-pilot profile does this.
        let ids: Vec<&str> = coordinator_section_ids()
            .into_iter()
            .filter(|id| *id != "routing_and_dispatch")
            .collect();
        let assembled = assemble_prompt(&ids, None).expect("assembly should succeed");

        // Routing-only headings must be gone.
        assert!(
            !assembled.contains("## Delegation Format"),
            "excluded routing section should not appear: {assembled}",
        );
        assert!(
            !assembled.contains("## Tasklist Format"),
            "excluded routing section should not appear: {assembled}",
        );
        assert!(
            !assembled.contains("## Round Limit"),
            "excluded routing section should not appear: {assembled}",
        );
        assert!(
            !assembled.contains("<delegation"),
            "excluded routing section's tag examples should not appear",
        );

        // Sibling sections still present.
        assert!(assembled.contains("## Team Coordination Role"));
        assert!(assembled.contains("## Conversation Style"));
    }

    #[test]
    fn identity_override_replaces_identity_body() {
        let ids = vec!["identity", "conversation_style"];
        let override_text = "## Identity\n\nYou are this tasklist's co-pilot.";
        let assembled =
            assemble_prompt(&ids, Some(override_text)).expect("assembly should succeed");

        assert!(
            assembled.contains("You are this tasklist's co-pilot."),
            "override body should appear in assembled prompt: {assembled}",
        );
        assert!(
            !assembled.contains("## Team Coordination Role"),
            "default identity heading should be replaced by override: {assembled}",
        );
        assert!(
            !assembled.contains("{{team_name}}"),
            "default identity body (with team_name placeholder) should be gone: {assembled}",
        );
        // The conversation_style section is still present, untouched.
        assert!(assembled.contains("## Conversation Style"));
    }

    #[test]
    fn identity_override_ignored_when_identity_section_absent() {
        // No identity in the list — override should be a no-op rather than
        // being injected into some other section.
        let ids = vec!["conversation_style", "tools_reference"];
        let override_text = "## Identity\n\nignored override";
        let assembled =
            assemble_prompt(&ids, Some(override_text)).expect("assembly should succeed");

        assert!(
            !assembled.contains("ignored override"),
            "override should not be injected when identity is not in the section list: {assembled}",
        );
        assert!(assembled.contains("## Conversation Style"));
        assert!(assembled.contains("## Tools Reference"));
    }

    #[test]
    fn unknown_section_id_produces_clear_error() {
        let ids = vec!["identity", "not_a_real_section", "conversation_style"];
        let err = assemble_prompt(&ids, None).expect_err("should error on unknown id");
        assert_eq!(err, AssembleError::UnknownSection("not_a_real_section".to_string()));
        // Error message names the offending id so a typo is easy to spot.
        assert!(format!("{err}").contains("not_a_real_section"));
    }

    #[test]
    fn empty_section_list_assembles_to_empty_string() {
        let assembled = assemble_prompt(&[], None).expect("empty assembly is valid");
        assert!(assembled.is_empty());
    }

    /// CLI-agent profiles compose from `CLI_AGENT_SECTION_IDS` and must
    /// document the `<task-item-notification>` block so producing agents
    /// know the contract.
    #[test]
    fn cli_agent_assembly_documents_task_item_notification_block() {
        use crate::prompt_sections::CLI_AGENT_SECTION_IDS;

        let assembled = assemble_prompt(CLI_AGENT_SECTION_IDS, None)
            .expect("CLI-agent assembly should succeed");

        // The block tag string itself appears.
        assert!(
            assembled.contains("<task-item-notification>"),
            "assembled CLI-agent prompt should contain the documented block tag: {assembled}",
        );
        assert!(
            assembled.contains("</task-item-notification>"),
            "assembled CLI-agent prompt should contain the closing tag: {assembled}",
        );

        // Payload field names are documented.
        assert!(assembled.contains("status"));
        assert!(assembled.contains("summary"));
        assert!(assembled.contains("details"));

        // The placement rule (notification must be NESTED inside the wrapping
        // `<task action="…">` tag, not a sibling) is present so producing
        // agents know the new contract — and that the legacy self-closing
        // `<task />` form is no longer accepted for complete/fail.
        let lower = assembled.to_lowercase();
        assert!(
            lower.contains("nested"),
            "nested placement rule should appear in the assembled prompt: {assembled}",
        );
        assert!(
            lower.contains("as its body") || lower.contains("as the body"),
            "placement rule should describe the notification as the body of the task tag: {assembled}",
        );

        // CLI agents do not dispatch — routing-only headings must not appear.
        assert!(
            !assembled.contains("## Delegation Format"),
            "CLI-agent prompt should not include the routing/dispatch section",
        );
    }

    /// The seeded `tasklist-copilot` profile excludes the routing/dispatch
    /// section, applies the co-pilot identity override, and uses the
    /// heavily-emphasized conversation-style variant.
    #[test]
    fn copilot_profile_excludes_routing_and_keeps_conversation_emphasis() {
        let assembled = assemble_copilot_prompt().expect("co-pilot assembly should succeed");

        // Routing/dispatch surface is gone — the co-pilot does not delegate.
        assert!(
            !assembled.contains("## Delegation Format"),
            "co-pilot prompt should not include the routing section: {assembled}",
        );
        assert!(
            !assembled.contains("## Tasklist Format"),
            "co-pilot prompt should not include the coordinator tasklist authoring section: {assembled}",
        );
        assert!(
            !assembled.contains("## Round Limit"),
            "co-pilot prompt should not include the round limit section: {assembled}",
        );
        assert!(
            !assembled.contains("<delegation agent="),
            "co-pilot prompt should not include the delegation tag example: {assembled}",
        );
        // The co-pilot composition documents `<tasklist action="append">`
        // (and only append) so the co-pilot can add work to the bound
        // tasklist. The append path resolves that binding via
        // `find_by_copilot_agent_id`, which walks both the team and agent
        // tasklist trees — see
        // `test_find_by_copilot_agent_id_resolves_agent_owned_tasklists`,
        // which guards the agent-owned half that the live project co-pilot
        // route depends on.
        //
        // The coordinator's `action="create"` example must remain excluded —
        // team-scoped creation is no longer supported and the append branch
        // rejects it explicitly.
        assert!(
            assembled.contains("<tasklist action=\"append\""),
            "co-pilot prompt should document the append tasklist tag: {assembled}",
        );
        assert!(
            !assembled.contains("<tasklist action=\"create\""),
            "co-pilot prompt should not include the coordinator's create tasklist example: {assembled}",
        );

        // Identity override is in place: co-pilot wording present, default
        // coordinator identity heading + placeholder are gone.
        let lower = assembled.to_lowercase();
        assert!(
            lower.contains("you are this tasklist's co-pilot"),
            "co-pilot prompt should carry the co-pilot identity override: {assembled}",
        );
        assert!(
            !assembled.contains("## Team Coordination Role"),
            "default coordinator identity heading should be replaced by the override: {assembled}",
        );
        assert!(
            !assembled.contains("{{team_name}}"),
            "default identity body's team_name placeholder should be gone: {assembled}",
        );

        // Heavy conversation-style emphasis is the co-pilot's signature
        // tone marker — the variant section's lead bold sentence.
        assert!(
            assembled.contains("**Conversation is your primary mode of work.**"),
            "co-pilot prompt should carry the conversation-style emphasis: {assembled}",
        );

        // Tools section is the co-pilot variant: investigation framing +
        // `remindMe = self` default.
        assert!(
            assembled.contains("investigation"),
            "co-pilot tools section should describe investigation framing: {assembled}",
        );
        assert!(
            assembled.contains("remindMe = self"),
            "co-pilot tools section should document the remindMe default: {assembled}",
        );

        // Sibling sections that ARE in the co-pilot composition still appear.
        assert!(assembled.contains("## Task Lifecycle"));
        assert!(assembled.contains("## Task Notification Format"));
        assert!(assembled.contains("## Conversation Context"));
    }

    /// Assembling the co-pilot profile twice produces identical output —
    /// the "profile registration" is the const data the function reads, so
    /// re-invocation never duplicates or drifts.
    #[test]
    fn copilot_profile_assembly_is_idempotent() {
        let a = assemble_copilot_prompt().expect("first assembly");
        let b = assemble_copilot_prompt().expect("second assembly");
        assert_eq!(a, b);
    }
}
