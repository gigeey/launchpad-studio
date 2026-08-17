use std::sync::LazyLock;

use regex::Regex;

static CODE_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)```[^\n]*\n.*?```").expect("code block regex should compile")
});

static TASK_COMMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<task_comment\s+([^>]+)>\s*(.*?)\s*</task_comment>"#)
        .expect("task_comment regex should compile")
});

static TASKLIST_ID_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"tasklist_id="([^"]+)""#).expect("tasklist_id attr regex should compile")
});

static TASK_ID_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"task_id="([^"]+)""#).expect("task_id attr regex should compile")
});

/// A `<task_comment>` block parsed out of coordinator output. The coordinator
/// emits these in place of calling the HTTP comments endpoint; the team queue
/// manager applies them in-process via the persistence layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCommentRequest {
    pub tasklist_id: String,
    pub task_id: String,
    pub body: String,
}

pub fn has_task_comments(text: &str) -> bool {
    text.contains("<task_comment ")
}

fn code_block_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    CODE_BLOCK_RE
        .find_iter(text)
        .map(|m| m.start()..m.end())
        .collect()
}

fn in_code_block(offset: usize, ranges: &[std::ops::Range<usize>]) -> bool {
    ranges.iter().any(|r| r.contains(&offset))
}

/// Extract `<task_comment tasklist_id="..." task_id="...">body</task_comment>`
/// blocks from coordinator output. Tags inside fenced code blocks are ignored.
/// Malformed tags (missing required attributes) are left in place and skipped.
pub fn extract_task_comments(text: &str) -> (String, Vec<TaskCommentRequest>) {
    let code_blocks = code_block_ranges(text);
    let mut comments = Vec::new();

    let cleaned = TASK_COMMENT_RE
        .replace_all(text, |caps: &regex::Captures| {
            let full_match = caps.get(0).unwrap();

            if in_code_block(full_match.start(), &code_blocks) {
                return full_match.as_str().to_string();
            }

            let attrs = &caps[1];
            let body = caps[2].trim().to_string();

            let tasklist_id = TASKLIST_ID_ATTR_RE.captures(attrs).map(|c| c[1].to_string());
            let task_id = TASK_ID_ATTR_RE.captures(attrs).map(|c| c[1].to_string());

            let (Some(tasklist_id), Some(task_id)) = (tasklist_id, task_id) else {
                return full_match.as_str().to_string();
            };

            if body.is_empty() {
                return full_match.as_str().to_string();
            }

            comments.push(TaskCommentRequest {
                tasklist_id,
                task_id,
                body,
            });

            String::new()
        })
        .to_string();

    (cleaned.trim().to_string(), comments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_comment() {
        let text = r#"Routing the task.
<task_comment tasklist_id="tl-1" task_id="t-1">
Recommend assigning to: writer.
</task_comment>"#;
        let (cleaned, comments) = extract_task_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].tasklist_id, "tl-1");
        assert_eq!(comments[0].task_id, "t-1");
        assert_eq!(comments[0].body, "Recommend assigning to: writer.");
        assert_eq!(cleaned, "Routing the task.");
    }

    #[test]
    fn extracts_multiple_comments() {
        let text = r#"<task_comment tasklist_id="tl-1" task_id="t-a">First note.</task_comment>
between
<task_comment tasklist_id="tl-1" task_id="t-b">Second note.</task_comment>"#;
        let (cleaned, comments) = extract_task_comments(text);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].task_id, "t-a");
        assert_eq!(comments[1].task_id, "t-b");
        assert!(cleaned.contains("between"));
        assert!(!cleaned.contains("<task_comment"));
    }

    #[test]
    fn no_comments() {
        let text = "Just text, no tags.";
        let (cleaned, comments) = extract_task_comments(text);
        assert!(comments.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn has_task_comments_detects_open_tag() {
        assert!(has_task_comments(
            r#"<task_comment tasklist_id="x" task_id="y">b</task_comment>"#
        ));
        assert!(!has_task_comments("nothing here"));
    }

    #[test]
    fn missing_required_attributes_skipped() {
        let text_no_tasklist = r#"<task_comment task_id="t-1">body</task_comment>"#;
        let (_, comments) = extract_task_comments(text_no_tasklist);
        assert!(comments.is_empty());

        let text_no_task = r#"<task_comment tasklist_id="tl-1">body</task_comment>"#;
        let (_, comments) = extract_task_comments(text_no_task);
        assert!(comments.is_empty());
    }

    #[test]
    fn empty_body_skipped() {
        let text = r#"<task_comment tasklist_id="tl-1" task_id="t-1"></task_comment>"#;
        let (_, comments) = extract_task_comments(text);
        assert!(comments.is_empty());
    }

    #[test]
    fn ignores_tags_inside_code_blocks() {
        let text = r#"Example:
```xml
<task_comment tasklist_id="tl-fake" task_id="t-fake">in code block</task_comment>
```
Real:
<task_comment tasklist_id="tl-real" task_id="t-real">real note</task_comment>"#;
        let (cleaned, comments) = extract_task_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].tasklist_id, "tl-real");
        assert!(cleaned.contains("```xml"));
        assert!(cleaned.contains("tl-fake"));
    }

    #[test]
    fn reversed_attribute_order() {
        let text =
            r#"<task_comment task_id="t-1" tasklist_id="tl-1">reversed order</task_comment>"#;
        let (_, comments) = extract_task_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].tasklist_id, "tl-1");
        assert_eq!(comments[0].task_id, "t-1");
        assert_eq!(comments[0].body, "reversed order");
    }

    #[test]
    fn extra_unknown_attributes_tolerated() {
        let text = r#"<task_comment tasklist_id="tl-1" task_id="t-1" priority="high">body</task_comment>"#;
        let (_, comments) = extract_task_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "body");
    }
}
