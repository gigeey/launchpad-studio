use std::sync::LazyLock;

use ao_protocol::delegation::DelegationRequest;
use regex::Regex;

/// Matches fenced code blocks (``` ... ```)
static CODE_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)```[^\n]*\n.*?```").expect("code block regex should compile")
});

/// Matches <delegation ...>...</delegation> blocks with any attributes
static DELEGATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<delegation\s+([^>]+)>\s*(.*?)\s*</delegation>"#)
        .expect("delegation regex should compile")
});

/// Extracts the agent attribute value from a delegation opening tag
static AGENT_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"agent="([^"]+)""#).expect("agent attr regex should compile")
});

/// Extracts the task_id attribute value from a delegation opening tag
static TASK_ID_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"task_id="([^"]+)""#).expect("task_id attr regex should compile")
});

/// Extracts the optional working_dir attribute value from a delegation opening tag
static WORKING_DIR_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"working_dir="([^"]+)""#).expect("working_dir attr regex should compile")
});

static PRIOR_CONTEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<prior_context>(.*?)</prior_context>"#).expect("prior_context regex")
});

/// Check if text contains any delegation tags.
pub fn has_delegations(text: &str) -> bool {
    // Quick check without full parsing
    text.contains("<delegation ")
}

/// Compute byte ranges of fenced code blocks in the text.
fn code_block_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    CODE_BLOCK_RE
        .find_iter(text)
        .map(|m| m.start()..m.end())
        .collect()
}

/// Check if a byte offset falls within any code block range.
fn in_code_block(offset: usize, ranges: &[std::ops::Range<usize>]) -> bool {
    ranges.iter().any(|r| r.contains(&offset))
}

/// Extract delegation tags from coordinator output text.
/// Returns (cleaned_text, Vec<DelegationRequest>) where cleaned_text has delegation tags removed.
/// Delegation tags inside fenced code blocks are ignored.
pub fn extract_delegations(text: &str) -> (String, Vec<DelegationRequest>) {
    let code_blocks = code_block_ranges(text);
    let mut delegations = Vec::new();

    let cleaned = DELEGATION_RE
        .replace_all(text, |caps: &regex::Captures| {
            let full_match = caps.get(0).unwrap();

            // Skip delegation tags inside code blocks
            if in_code_block(full_match.start(), &code_blocks) {
                return full_match.as_str().to_string();
            }

            let attrs = &caps[1];
            let body = caps[2].to_string();

            // Extract agent and task_id attributes (order-independent)
            let agent = AGENT_ATTR_RE.captures(attrs).map(|c| c[1].to_string());
            let task_id = TASK_ID_ATTR_RE.captures(attrs).map(|c| c[1].to_string());

            // Both attributes are required
            let (Some(target_agent_id), Some(delegation_id)) = (agent, task_id) else {
                return full_match.as_str().to_string();
            };

            // Extract optional working_dir attribute
            let working_dir = WORKING_DIR_ATTR_RE
                .captures(attrs)
                .map(|c| c[1].to_string());

            // Extract prior_context if present
            let prior_context = PRIOR_CONTEXT_RE
                .captures(&body)
                .map(|pc| pc[1].trim().to_string());

            // Task is the body with prior_context tags removed
            let task = PRIOR_CONTEXT_RE
                .replace_all(&body, "")
                .trim()
                .to_string();

            delegations.push(DelegationRequest {
                delegation_id,
                target_agent_id,
                task,
                prior_context,
                working_dir,
            });

            // Replace delegation tag with empty string
            String::new()
        })
        .to_string();

    // Clean up extra blank lines left by removed tags
    let cleaned = cleaned.trim().to_string();

    (cleaned, delegations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_delegation() {
        let text = r#"Let me delegate this task.
<delegation agent="researcher" task_id="d-abc123">
Research quantum error correction papers from 2025.
</delegation>"#;

        let (cleaned, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].target_agent_id, "researcher");
        assert_eq!(delegations[0].delegation_id, "d-abc123");
        assert_eq!(
            delegations[0].task,
            "Research quantum error correction papers from 2025."
        );
        assert!(delegations[0].prior_context.is_none());
        assert_eq!(cleaned, "Let me delegate this task.");
    }

    #[test]
    fn test_multiple_delegations() {
        let text = r#"I'll assign these tasks:
<delegation agent="researcher" task_id="d-001">
Research topic A.
</delegation>
Some middle text.
<delegation agent="writer" task_id="d-002">
Write a summary of topic B.
</delegation>"#;

        let (cleaned, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 2);
        assert_eq!(delegations[0].target_agent_id, "researcher");
        assert_eq!(delegations[0].delegation_id, "d-001");
        assert_eq!(delegations[1].target_agent_id, "writer");
        assert_eq!(delegations[1].delegation_id, "d-002");
        assert!(cleaned.contains("I'll assign these tasks:"));
        assert!(cleaned.contains("Some middle text."));
        assert!(!cleaned.contains("<delegation"));
    }

    #[test]
    fn test_no_delegations() {
        let text = "Just a normal response with no delegation tags.";
        let (cleaned, delegations) = extract_delegations(text);
        assert!(delegations.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn test_has_delegations() {
        assert!(has_delegations(
            r#"<delegation agent="x" task_id="y">task</delegation>"#
        ));
        assert!(!has_delegations("no delegations here"));
    }

    #[test]
    fn test_malformed_tags() {
        // Missing closing tag
        let text = r#"<delegation agent="x" task_id="y">unclosed task"#;
        let (cleaned, delegations) = extract_delegations(text);
        assert!(delegations.is_empty());
        assert_eq!(cleaned, text);

        // Missing attributes
        let text2 = "<delegation>no attributes</delegation>";
        let (cleaned2, delegations2) = extract_delegations(text2);
        assert!(delegations2.is_empty());
        assert_eq!(cleaned2, text2);
    }

    #[test]
    fn test_prior_context_extraction() {
        let text = r#"<delegation agent="researcher" task_id="d-abc123">
Research quantum error correction papers from 2025.
<prior_context>Previously found 12 papers but IEEE was blocked.</prior_context>
</delegation>"#;

        let (_, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert_eq!(
            delegations[0].task,
            "Research quantum error correction papers from 2025."
        );
        assert_eq!(
            delegations[0].prior_context.as_deref(),
            Some("Previously found 12 papers but IEEE was blocked.")
        );
    }

    #[test]
    fn test_code_blocks_with_delegation_like_text() {
        // Delegation tags inside code blocks should be skipped
        let text = r#"Here's how delegation tags work:

```xml
<delegation agent="example" task_id="demo">This is just an example</delegation>
```

Now let me actually delegate:
<delegation agent="researcher" task_id="d-real">
Do the real research.
</delegation>"#;

        let (cleaned, delegations) = extract_delegations(text);
        // Only the real delegation outside code block should be extracted
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].delegation_id, "d-real");
        assert_eq!(delegations[0].target_agent_id, "researcher");
        // Code block should be preserved in cleaned text
        assert!(cleaned.contains("```xml"));
        assert!(cleaned.contains(r#"<delegation agent="example""#));
    }

    #[test]
    fn test_reversed_attribute_order() {
        let text = r#"<delegation task_id="d-reverse" agent="writer">
Write the report.
</delegation>"#;

        let (cleaned, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].target_agent_id, "writer");
        assert_eq!(delegations[0].delegation_id, "d-reverse");
        assert_eq!(delegations[0].task, "Write the report.");
        assert_eq!(cleaned, "");
    }

    #[test]
    fn test_extra_whitespace_between_attributes() {
        let text = r#"<delegation   agent="researcher"    task_id="d-spaced"  >
Do some research.
</delegation>"#;

        let (_, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].target_agent_id, "researcher");
        assert_eq!(delegations[0].delegation_id, "d-spaced");
        assert_eq!(delegations[0].task, "Do some research.");
    }

    #[test]
    fn test_extra_unknown_attributes() {
        // Extra attributes should be tolerated (ignored)
        let text = r#"<delegation agent="coder" priority="high" task_id="d-extra" timeout="30">
Implement the feature.
</delegation>"#;

        let (_, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].target_agent_id, "coder");
        assert_eq!(delegations[0].delegation_id, "d-extra");
        assert_eq!(delegations[0].task, "Implement the feature.");
    }

    #[test]
    fn test_working_dir_attribute() {
        let text = r#"<delegation agent="fe-agent" task_id="d-wd1" working_dir="/home/dev/repos/app">
Build the login page.
</delegation>"#;

        let (_, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].target_agent_id, "fe-agent");
        assert_eq!(delegations[0].delegation_id, "d-wd1");
        assert_eq!(delegations[0].task, "Build the login page.");
        assert_eq!(
            delegations[0].working_dir.as_deref(),
            Some("/home/dev/repos/app")
        );
    }

    #[test]
    fn test_working_dir_absent() {
        let text = r#"<delegation agent="researcher" task_id="d-nowd">
Research something.
</delegation>"#;

        let (_, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert!(delegations[0].working_dir.is_none());
    }

    #[test]
    fn test_delegation_inside_multiple_code_blocks() {
        let text = r#"Some text before.

```
<delegation agent="fake1" task_id="d-fake1">Fake task 1</delegation>
```

Real delegation:
<delegation agent="real" task_id="d-real">Real task</delegation>

```python
# <delegation agent="fake2" task_id="d-fake2">Fake task 2</delegation>
```"#;

        let (cleaned, delegations) = extract_delegations(text);
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].delegation_id, "d-real");
        assert!(cleaned.contains("Some text before."));
        assert!(cleaned.contains("Real delegation:"));
        // Code blocks preserved
        assert!(cleaned.contains("fake1"));
        assert!(cleaned.contains("fake2"));
    }
}
