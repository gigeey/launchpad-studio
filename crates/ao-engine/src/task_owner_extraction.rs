use std::sync::LazyLock;

use ao_protocol::team::TeamMember;
use regex::Regex;

/// Outcome of parsing the routing classifier's `<task_owner>` tag.
///
/// `agent_id` is the validated owner pulled from the team roster.
/// `expected_outputs` is the optional comma-separated list the classifier may
/// emit on the tag (`<task_owner expected_outputs="a.md,b.md">…`). Empty when
/// the classifier omitted the attribute or the value was blank — callers
/// should leave the task's existing `expected_outputs` untouched in that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOwnerDecision {
    pub agent_id: String,
    pub expected_outputs: Vec<String>,
}

static TASK_OWNER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Optional `expected_outputs="csv"` attribute precedes the closing `>`.
    // The `(?s)` flag lets `.` match newlines so the body can span lines.
    Regex::new(
        r#"(?s)<task_owner(?:\s+expected_outputs="([^"]*)")?\s*>\s*(.*?)\s*</task_owner>"#,
    )
    .expect("task_owner regex should compile")
});

/// Extract a validated routing decision from the first
/// `<task_owner>...</task_owner>` tag in the classifier's output.
///
/// The tag may carry an optional `expected_outputs="file1,file2"` attribute
/// the classifier infers from the task prompt (e.g. when the prompt asks the
/// agent to write a file). The attribute value is split on commas, trimmed,
/// and empty entries are dropped — `expected_outputs=""` and a missing
/// attribute both yield an empty `expected_outputs` vec.
///
/// Returns `Some(decision)` only when the parsed `agent_id` (after trimming
/// surrounding whitespace) matches a member's `agent_id` exactly. Returns
/// `None` for: missing tag, empty/whitespace-only tag body, or an id that
/// does not appear in `members`.
///
/// When the LLM emits multiple `<task_owner>` tags, the **first** tag wins —
/// classifier output is expected to contain at most one decision tag, and
/// preferring the first keeps behavior deterministic against truncation.
pub fn extract_task_owner(text: &str, members: &[TeamMember]) -> Option<TaskOwnerDecision> {
    let caps = TASK_OWNER_RE.captures(text)?;
    let raw = caps.get(2)?.as_str().trim();
    if raw.is_empty() {
        return None;
    }
    let agent_id = members.iter().find(|m| m.agent_id == raw)?.agent_id.clone();

    let expected_outputs = caps
        .get(1)
        .map(|m| {
            m.as_str()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(TaskOwnerDecision {
        agent_id,
        expected_outputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> Vec<TeamMember> {
        vec![
            TeamMember {
                agent_id: "researcher".to_string(),
                role_description: "Researches".to_string(),
                working_dir: None,
            },
            TeamMember {
                agent_id: "writer".to_string(),
                role_description: "Writes".to_string(),
                working_dir: None,
            },
        ]
    }

    fn decision(agent_id: &str, outputs: &[&str]) -> TaskOwnerDecision {
        TaskOwnerDecision {
            agent_id: agent_id.to_string(),
            expected_outputs: outputs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn well_formed_valid_agent() {
        let text = "Picking owner: <task_owner>writer</task_owner>";
        assert_eq!(extract_task_owner(text, &members()), Some(decision("writer", &[])));
    }

    #[test]
    fn well_formed_unknown_agent() {
        let text = "<task_owner>marketer</task_owner>";
        assert_eq!(extract_task_owner(text, &members()), None);
    }

    #[test]
    fn missing_tag() {
        let text = "I don't know who should own this.";
        assert_eq!(extract_task_owner(text, &members()), None);
    }

    #[test]
    fn empty_tag_body() {
        let text = "<task_owner></task_owner>";
        assert_eq!(extract_task_owner(text, &members()), None);
    }

    #[test]
    fn whitespace_only_tag_body() {
        let text = "<task_owner>   </task_owner>";
        assert_eq!(extract_task_owner(text, &members()), None);
    }

    #[test]
    fn malformed_tag_unclosed() {
        let text = "<task_owner>writer";
        assert_eq!(extract_task_owner(text, &members()), None);
    }

    #[test]
    fn malformed_tag_open_only() {
        let text = "<task_owner>";
        assert_eq!(extract_task_owner(text, &members()), None);
    }

    #[test]
    fn whitespace_around_id_is_trimmed() {
        let text = "<task_owner>   researcher   </task_owner>";
        assert_eq!(
            extract_task_owner(text, &members()),
            Some(decision("researcher", &[]))
        );
    }

    #[test]
    fn newlines_around_id_are_trimmed() {
        let text = "<task_owner>\n  writer  \n</task_owner>";
        assert_eq!(
            extract_task_owner(text, &members()),
            Some(decision("writer", &[]))
        );
    }

    #[test]
    fn multiple_tags_first_wins() {
        // Documented behavior: the FIRST <task_owner> tag is used.
        let text = "<task_owner>researcher</task_owner> then <task_owner>writer</task_owner>";
        assert_eq!(
            extract_task_owner(text, &members()),
            Some(decision("researcher", &[]))
        );
    }

    #[test]
    fn empty_member_list_rejects_everything() {
        let text = "<task_owner>writer</task_owner>";
        assert_eq!(extract_task_owner(text, &[]), None);
    }

    #[test]
    fn case_sensitive_id_match() {
        let text = "<task_owner>Writer</task_owner>";
        assert_eq!(extract_task_owner(text, &members()), None);
    }

    #[test]
    fn expected_outputs_single_file() {
        let text = r#"<task_owner expected_outputs="design.md">writer</task_owner>"#;
        assert_eq!(
            extract_task_owner(text, &members()),
            Some(decision("writer", &["design.md"]))
        );
    }

    #[test]
    fn expected_outputs_multiple_files_trimmed() {
        let text = r#"<task_owner expected_outputs=" a.md , b.txt ,c.json ">writer</task_owner>"#;
        assert_eq!(
            extract_task_owner(text, &members()),
            Some(decision("writer", &["a.md", "b.txt", "c.json"]))
        );
    }

    #[test]
    fn expected_outputs_empty_attribute_yields_empty_list() {
        let text = r#"<task_owner expected_outputs="">writer</task_owner>"#;
        assert_eq!(
            extract_task_owner(text, &members()),
            Some(decision("writer", &[]))
        );
    }

    #[test]
    fn expected_outputs_drops_empty_entries() {
        let text = r#"<task_owner expected_outputs=",a.md,,b.md,">writer</task_owner>"#;
        assert_eq!(
            extract_task_owner(text, &members()),
            Some(decision("writer", &["a.md", "b.md"]))
        );
    }

    #[test]
    fn expected_outputs_invalid_owner_still_rejected() {
        // The attribute should not bypass roster validation.
        let text = r#"<task_owner expected_outputs="a.md">marketer</task_owner>"#;
        assert_eq!(extract_task_owner(text, &members()), None);
    }
}
