//! Coordinator-family prompt sections, loaded from `crates/ao-engine/prompts/sections/`.
//!
//! Each section is a self-contained markdown file that can be assembled into a
//! coordinator-family system prompt by composing an ordered list of section
//! ids. The assembler that performs the composition lives in a follow-up
//! story; this module exposes the raw section content and the canonical
//! coordinator section ordering.

/// Canonical section id and its `include_str!` body.
pub struct PromptSection {
    pub id: &'static str,
    pub body: &'static str,
}

pub const IDENTITY: PromptSection = PromptSection {
    id: "identity",
    body: include_str!("../prompts/sections/identity.md"),
};

pub const TASK_LIFECYCLE: PromptSection = PromptSection {
    id: "task_lifecycle",
    body: include_str!("../prompts/sections/task_lifecycle.md"),
};

pub const TASK_NOTIFICATION_FORMAT: PromptSection = PromptSection {
    id: "task_notification_format",
    body: include_str!("../prompts/sections/task_notification_format.md"),
};

pub const ROUTING_AND_DISPATCH: PromptSection = PromptSection {
    id: "routing_and_dispatch",
    body: include_str!("../prompts/sections/routing_and_dispatch.md"),
};

pub const TOOLS_REFERENCE: PromptSection = PromptSection {
    id: "tools_reference",
    body: include_str!("../prompts/sections/tools_reference.md"),
};

pub const CONVERSATION_STYLE: PromptSection = PromptSection {
    id: "conversation_style",
    body: include_str!("../prompts/sections/conversation_style.md"),
};

pub const CONTEXT_INJECTION: PromptSection = PromptSection {
    id: "context_injection",
    body: include_str!("../prompts/sections/context_injection.md"),
};

/// All registered sections. The order here is also the canonical coordinator
/// composition order — assemblers that want a coordinator profile can use
/// this list as-is.
pub const ALL_SECTIONS: &[&PromptSection] = &[
    &IDENTITY,
    &TASK_LIFECYCLE,
    &TASK_NOTIFICATION_FORMAT,
    &ROUTING_AND_DISPATCH,
    &TOOLS_REFERENCE,
    &CONVERSATION_STYLE,
    &CONTEXT_INJECTION,
];

/// Co-pilot variant of `tools_reference` — talks about investigation tasks
/// and `remindMe = self` rather than the generic neutral-stub wording.
/// Lives in a sibling file so the coordinator and co-pilot profiles can
/// each pick the variant that fits without cross-references.
pub const TOOLS_REFERENCE_COPILOT: PromptSection = PromptSection {
    id: "tools_reference_copilot",
    body: include_str!("../prompts/sections/tools_reference.copilot.md"),
};

/// Co-pilot variant of `conversation_style` — heavy emphasis on
/// conversation as the primary mode of work, since the co-pilot does not
/// dispatch other agents.
pub const CONVERSATION_STYLE_COPILOT: PromptSection = PromptSection {
    id: "conversation_style_copilot",
    body: include_str!("../prompts/sections/conversation_style.copilot.md"),
};

/// Co-pilot variant sections registered for `lookup` but intentionally
/// kept out of `ALL_SECTIONS` so the canonical coordinator composition
/// order is unaffected.
pub const COPILOT_VARIANT_SECTIONS: &[&PromptSection] = &[
    &TOOLS_REFERENCE_COPILOT,
    &CONVERSATION_STYLE_COPILOT,
];

/// Canonical section ids for a CLI-agent profile (a non-coordinator agent
/// that works individual tasks dispatched by the TaskFeeder). CLI agents
/// must know how to emit the `<task-item-notification>` block, so
/// `task_notification_format` is included; `routing_and_dispatch` and
/// `context_injection` are excluded because CLI agents do not dispatch
/// other agents and do not receive a member roster.
pub const CLI_AGENT_SECTION_IDS: &[&str] = &[
    "identity",
    "task_lifecycle",
    "task_notification_format",
    "tools_reference",
    "conversation_style",
];

/// Profile id for the seeded tasklist co-pilot.
pub const COPILOT_PROFILE_ID: &str = "tasklist-copilot";

/// Identity-section override applied when assembling the co-pilot profile.
/// Replaces the default coordinator identity body verbatim — the override
/// supplies its own `## ` heading.
pub const COPILOT_IDENTITY_OVERRIDE: &str =
    include_str!("../prompts/sections/identity.copilot.md");

/// Canonical section ids for the `tasklist-copilot` profile. Excludes
/// `routing_and_dispatch` (the co-pilot does not dispatch) and uses the
/// `*_copilot` variants of `tools_reference` and `conversation_style`.
pub const COPILOT_SECTION_IDS: &[&str] = &[
    "identity",
    "task_lifecycle",
    "task_notification_format",
    "tools_reference_copilot",
    "conversation_style_copilot",
    "context_injection",
];

/// Look up a section by id. Returns `None` for unknown ids — the assembler
/// in a follow-up story is responsible for surfacing this as an error.
pub fn lookup(id: &str) -> Option<&'static PromptSection> {
    ALL_SECTIONS
        .iter()
        .chain(COPILOT_VARIANT_SECTIONS.iter())
        .copied()
        .find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every section file must be non-empty.
    #[test]
    fn every_section_has_non_empty_body() {
        for s in ALL_SECTIONS {
            assert!(
                !s.body.trim().is_empty(),
                "section `{}` has an empty body",
                s.id
            );
        }
    }

    /// Section ids must be unique.
    #[test]
    fn section_ids_are_unique() {
        let mut ids: Vec<&str> = ALL_SECTIONS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(len_before, ids.len(), "duplicate section id detected");
    }

    /// `lookup` resolves known ids and rejects unknown ones.
    #[test]
    fn lookup_known_and_unknown() {
        for s in ALL_SECTIONS {
            assert!(lookup(s.id).is_some(), "lookup missed `{}`", s.id);
        }
        assert!(lookup("not_a_real_section").is_none());
    }

    /// Sections must be self-contained: no "as mentioned above" / "see
    /// section X" cross-references that would dangle when a profile excludes
    /// a sibling section.
    #[test]
    fn no_cross_references_between_sections() {
        let forbidden = [
            "as mentioned above",
            "as described above",
            "see section",
            "see above",
            "see below",
            "the section above",
            "the section below",
        ];
        for s in ALL_SECTIONS {
            let lower = s.body.to_lowercase();
            for needle in forbidden {
                assert!(
                    !lower.contains(needle),
                    "section `{}` contains forbidden cross-reference `{}`",
                    s.id,
                    needle
                );
            }
        }
    }

    /// Coordinator-prompt anchor strings must collectively appear across the
    /// section set so the assembled prompt covers everything the legacy
    /// `build_coordinator_system_prompt` produced (no information loss).
    #[test]
    fn anchors_from_legacy_coordinator_prompt_are_covered() {
        let combined: String = ALL_SECTIONS.iter().map(|s| s.body).collect();
        let anchors = [
            // Identity / role
            "coordinator of team",
            // Task lifecycle
            "<task action=\"complete\"",
            "expected_outputs",
            "automatic reprompt",
            "follow-up message",
            // Routing / dispatch
            "## Delegation Format",
            "<delegation agent=\"agent_id\" task_id=\"unique-task-id\" working_dir=\"/optional/path\">",
            "<prior_context>",
            "## Tasklist Format",
            "<tasklist action=\"create\"",
            "mode: PAR",
            "mode: SEQ",
            "owner_agent_id",
            // Round limit
            "## Round Limit",
            "{{max_delegation_rounds}}",
            // Context injection placeholders for focus dir / roster
            "{{focus_directory_block}}",
            "{{member_roster_block}}",
            // Templated team identifiers used by the assembler
            "{{team_name}}",
            "{{team_id}}",
        ];
        for a in anchors {
            assert!(
                combined.contains(a),
                "section set missing anchor `{}`",
                a
            );
        }
    }

    /// Each section opens with a markdown `##` heading so the assembled
    /// output reads as a coherent multi-section prompt.
    #[test]
    fn every_section_starts_with_h2_heading() {
        for s in ALL_SECTIONS {
            let first = s.body.trim_start().lines().next().unwrap_or("");
            assert!(
                first.starts_with("## "),
                "section `{}` should start with an `## ` heading; first line was `{}`",
                s.id,
                first
            );
        }
    }

    /// Co-pilot variant sections are looked up by `assemble_prompt`, so they
    /// must satisfy the same hygiene checks as `ALL_SECTIONS`.
    #[test]
    fn copilot_variant_sections_are_well_formed() {
        for s in COPILOT_VARIANT_SECTIONS {
            assert!(
                !s.body.trim().is_empty(),
                "variant section `{}` has an empty body",
                s.id
            );
            let first = s.body.trim_start().lines().next().unwrap_or("");
            assert!(
                first.starts_with("## "),
                "variant section `{}` should start with an `## ` heading; first line was `{}`",
                s.id,
                first
            );
            let lower = s.body.to_lowercase();
            for needle in [
                "as mentioned above",
                "as described above",
                "see section",
                "see above",
                "see below",
                "the section above",
                "the section below",
            ] {
                assert!(
                    !lower.contains(needle),
                    "variant section `{}` contains forbidden cross-reference `{}`",
                    s.id,
                    needle
                );
            }
        }
    }

    /// `lookup` ids must be unique across `ALL_SECTIONS` and the co-pilot
    /// variant set so a `lookup` call has exactly one possible match.
    #[test]
    fn lookup_ids_are_unique_across_all_registered_sections() {
        let mut ids: Vec<&str> = ALL_SECTIONS
            .iter()
            .chain(COPILOT_VARIANT_SECTIONS.iter())
            .map(|s| s.id)
            .collect();
        ids.sort_unstable();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(
            len_before,
            ids.len(),
            "duplicate section id detected across registered sections"
        );
    }

    /// Every id named by `COPILOT_SECTION_IDS` must resolve via `lookup` —
    /// otherwise the co-pilot assembly fails at runtime.
    #[test]
    fn copilot_section_ids_all_resolve() {
        for id in COPILOT_SECTION_IDS {
            assert!(
                lookup(id).is_some(),
                "co-pilot section id `{}` does not resolve via lookup",
                id
            );
        }
    }

    /// The co-pilot identity override must start with its own `## ` heading
    /// so the assembled prompt reads coherently when the override replaces
    /// the default identity body.
    #[test]
    fn copilot_identity_override_starts_with_h2_heading() {
        let first = COPILOT_IDENTITY_OVERRIDE
            .trim_start()
            .lines()
            .next()
            .unwrap_or("");
        assert!(
            first.starts_with("## "),
            "COPILOT_IDENTITY_OVERRIDE should start with an `## ` heading; first line was `{}`",
            first
        );
    }
}
