//! Permission rule grammar — parses `Tool(arg-glob)` strings (e.g.
//! `Bash(git *)`, `Read(/etc/**)`) into a [`PermissionRule`] and tests a
//! parsed rule against an inbound tool invocation.
//!
//! Two rule shapes are accepted:
//!
//! - `Tool` — bare tool-name match. Matches every invocation of that
//!   tool regardless of input.
//! - `Tool(arg-glob)` — tool-name plus a [`globset`] glob applied to a
//!   per-tool canonical input string (see [`rule_matches`] for the
//!   field map).

use ao_engine_tools_core::PermissionDecision;
use globset::{Glob, GlobMatcher};
use serde_json::Value;

/// A parsed permission rule. Construct via [`parse_rule`]; evaluate via
/// [`rule_matches`].
///
/// `arg_pattern` is `None` when the rule is bare (`Tool` form) and
/// matches every invocation of `tool_name`. When `Some`, the matcher is
/// compared against the per-tool canonical input string described on
/// [`rule_matches`].
#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub tool_name: String,
    pub arg_pattern: Option<GlobMatcher>,
    pub decision: PermissionDecision,
}

/// Errors returned by [`parse_rule`] for malformed rule strings.
#[derive(Debug, thiserror::Error)]
pub enum RuleParseError {
    /// The rule string was empty or whitespace-only.
    #[error("permission rule is empty")]
    Empty,

    /// The tool-name segment before the opening paren was empty (e.g.
    /// `(git *)` or `   (foo)`).
    #[error("permission rule has empty tool name")]
    EmptyToolName,

    /// The rule contained unbalanced parentheses — either a `(` with no
    /// matching `)`, a stray `)`, or trailing characters after the
    /// closing paren.
    #[error("permission rule has unbalanced parentheses: '{0}'")]
    UnbalancedParens(String),

    /// The argument segment between `(` and `)` is not a valid glob.
    #[error("permission rule '{rule}' has invalid glob: {source}")]
    InvalidGlob {
        rule: String,
        #[source]
        source: globset::Error,
    },
}

/// Parse a rule string of the form `Tool` or `Tool(arg-glob)` into a
/// [`PermissionRule`] tagged with `decision`. Returns
/// [`RuleParseError`] on malformed input.
///
/// Whitespace surrounding the tool name and around the rule itself is
/// trimmed; whitespace inside the glob is preserved verbatim so patterns
/// like `Bash(git *)` work.
pub fn parse_rule(
    raw: &str,
    decision: PermissionDecision,
) -> Result<PermissionRule, RuleParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(RuleParseError::Empty);
    }

    match trimmed.find('(') {
        None => {
            if trimmed.contains(')') {
                return Err(RuleParseError::UnbalancedParens(raw.to_string()));
            }
            Ok(PermissionRule {
                tool_name: trimmed.to_string(),
                arg_pattern: None,
                decision,
            })
        }
        Some(open_idx) => {
            if !trimmed.ends_with(')') {
                return Err(RuleParseError::UnbalancedParens(raw.to_string()));
            }
            let tool_name = trimmed[..open_idx].trim();
            if tool_name.is_empty() {
                return Err(RuleParseError::EmptyToolName);
            }
            let pattern = &trimmed[open_idx + 1..trimmed.len() - 1];
            let glob = Glob::new(pattern).map_err(|source| RuleParseError::InvalidGlob {
                rule: raw.to_string(),
                source,
            })?;
            Ok(PermissionRule {
                tool_name: tool_name.to_string(),
                arg_pattern: Some(glob.compile_matcher()),
                decision,
            })
        }
    }
}

/// Returns `true` when `rule` applies to an invocation of `tool_name`
/// with `input`.
///
/// A bare rule (no `arg_pattern`) matches every invocation of its
/// `tool_name`. A rule with a glob matches when the glob matches the
/// per-tool canonical input string:
///
/// - `Bash` → the `command` field
/// - `Read` / `Edit` / `Write` → the `file_path` field
/// - `WebFetch` → the `url` field
/// - any other tool → `serde_json::to_string(input)` (compact)
///
/// To extend the canonical-string map for a new tool, add a branch to
/// [`canonical_arg`].
pub fn rule_matches(rule: &PermissionRule, tool_name: &str, input: &Value) -> bool {
    if rule.tool_name != tool_name {
        return false;
    }
    let Some(matcher) = &rule.arg_pattern else {
        return true;
    };
    let canonical = canonical_arg(tool_name, input);
    matcher.is_match(&canonical)
}

fn canonical_arg(tool_name: &str, input: &Value) -> String {
    let field = match tool_name {
        "Bash" => Some("command"),
        "Read" | "Edit" | "Write" => Some("file_path"),
        "WebFetch" => Some("url"),
        "RunSkill" => Some("skill"),
        _ => None,
    };
    if let Some(field_name) = field {
        if let Some(s) = input.get(field_name).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    serde_json::to_string(input).unwrap_or_default()
}
