use ao_protocol::agent::DelegateTarget;

/// Build the (system_prompt, user_prompt) pair for the delegate-target classifier.
///
/// The prompt instructs the model to respond with exactly one JSON object:
///   { "owner_agent_id": "<one of the agent_ids listed, or null>" }
///
/// Null means "leave with the current (parent) agent". Any other string must
/// exactly match one of the provided `targets` target_agent_ids.
pub fn build_classify_prompt(
    targets: &[DelegateTarget],
    parent_system_prompt: Option<&str>,
    task_title: &str,
    task_description: &str,
) -> (String, String) {
    let mut system = String::new();

    if let Some(sp) = parent_system_prompt {
        system.push_str(sp);
        system.push_str("\n\n");
    }

    system.push_str(
        "You are a task routing assistant. Given a task and a list of available agents, \
decide which agent should handle the task.\n\n\
Respond with exactly one JSON object:\n\
{ \"owner_agent_id\": \"<agent_id or null>\" }\n\n\
Set owner_agent_id to one of the agent IDs listed in the user message, or null if the \
task should remain with the current agent. Do not include any other text.",
    );

    let mut user = String::new();
    user.push_str("## Available agents\n\n");
    for target in targets {
        user.push_str(&format!(
            "- {}: {} \u{2014} {}\n",
            target.target_agent_id, target.name, target.purpose
        ));
    }
    user.push_str("\n## Task to route\n\n");
    user.push_str(&format!("title: {}\n", task_title));
    if !task_description.is_empty() {
        user.push_str(&format!("description: {}\n", task_description));
    }
    user.push_str(
        "\nRespond with JSON only: { \"owner_agent_id\": \"<agent_id or null>\" }",
    );

    (system, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: &str, name: &str, desc: &str) -> DelegateTarget {
        DelegateTarget {
            target_agent_id: id.to_string(),
            name: name.to_string(),
            purpose: desc.to_string(),
            share_context_allowed: false,
        }
    }

    #[test]
    fn system_prompt_prefix_included_when_present() {
        let entries = vec![make_entry("backend", "Backend", "Handles API work")];
        let (sys, _user) =
            build_classify_prompt(&entries, Some("You are a planner."), "Do X", "");
        assert!(sys.starts_with("You are a planner."), "prefix missing: {}", &sys[..50.min(sys.len())]);
    }

    #[test]
    fn system_prompt_prefix_omitted_when_none() {
        let entries = vec![make_entry("backend", "Backend", "Handles API work")];
        let (sys, _user) = build_classify_prompt(&entries, None, "Do X", "");
        // No parent prefix means the system starts directly with routing instructions.
        assert!(!sys.starts_with("You are a planner"), "unexpected parent prefix: {}", &sys[..60.min(sys.len())]);
        assert!(sys.contains("task routing"), "routing instructions must be present: {}", &sys[..60.min(sys.len())]);
    }

    #[test]
    fn entries_formatted_with_em_dash() {
        let entries = vec![make_entry("backend", "Backend Agent", "Handles backend tasks")];
        let (_sys, user) = build_classify_prompt(&entries, None, "Do X", "");
        assert!(
            user.contains("backend: Backend Agent \u{2014} Handles backend tasks"),
            "em-dash format missing in: {}",
            user
        );
    }

    #[test]
    fn task_description_included_when_non_empty() {
        let entries = vec![make_entry("a", "A", "b")];
        let (_sys, user) = build_classify_prompt(&entries, None, "The Title", "The description");
        assert!(user.contains("The Title"), "title missing");
        assert!(user.contains("The description"), "description missing");
    }

    #[test]
    fn task_description_omitted_when_empty() {
        let entries = vec![make_entry("a", "A", "b")];
        let (_sys, user) = build_classify_prompt(&entries, None, "Only Title", "");
        assert!(user.contains("Only Title"), "title missing");
        assert!(!user.contains("description:"), "empty description should not appear");
    }
}
