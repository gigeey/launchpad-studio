/// One-shot migrator: parse a legacy system_prompt string and extract
/// persona / special_instructions content from it.
///
/// The migrator is pure — it performs no I/O and does not mutate any profile.
/// Callers are responsible for copying the raw prompt to
/// `profile.legacy_system_prompt` and persisting the extracted fields.
use serde::{Deserialize, Serialize};

/// Result of migrating a single legacy system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationResult {
    /// Descriptive persona content extracted from the legacy prompt.
    pub persona: Option<String>,
    /// Imperative instruction content extracted from the legacy prompt.
    pub special_instructions: Option<String>,
    /// Number of lines that were identified as boilerplate and stripped.
    pub stripped_boilerplate_lines: usize,
}

/// Parse `raw` and extract persona / special_instructions content,
/// stripping known boilerplate in the process.
///
/// Known boilerplate stripped:
/// - `<run-context>...</run-context>` XML blocks
/// - Static section blocks (baseline guidance, memory instructions, etc.)
/// - Section headers for Workflows, Delegate Targets, Agent Memories
/// - Lines consisting solely of `{{...}}` placeholder tokens
///
/// Classification heuristic (paragraph-level):
/// - Paragraphs where any line starts with an action verb
///   (Do, Don't, Always, Never, Avoid, When) → `special_instructions`
/// - All other paragraphs → `persona`
pub fn migrate_legacy_system_prompt(raw: &str) -> MigrationResult {
    if raw.trim().is_empty() {
        return MigrationResult {
            persona: None,
            special_instructions: None,
            stripped_boilerplate_lines: 0,
        };
    }

    let (kept, stripped_count) = strip_boilerplate(raw);
    let (persona, special_instructions) = classify_content(&kept);

    MigrationResult {
        persona: nonempty(persona),
        special_instructions: nonempty(special_instructions),
        stripped_boilerplate_lines: stripped_count,
    }
}

// ---------------------------------------------------------------------------
// Boilerplate stripping
// ---------------------------------------------------------------------------

/// Known top-level section headers that are pure boilerplate.
/// Content under these headers (until the next top-level section) is stripped.
static BOILERPLATE_SECTION_HEADERS: &[&str] = &[
    "# Tool Selection: Direct Tools vs. Sub-Agents",
    "# Memory Management",
    "# Conversation History Recall",
    "# Workflows",
    "# Delegate Targets",
    "[Agent Memories]",
    "# Agent Instructions",
    "# Agent Rules",
    "# Studio Skills",
    "# Workspace Instructions",
    "# Workspace Rules",
];

fn is_boilerplate_header(line: &str) -> bool {
    let t = line.trim();
    BOILERPLATE_SECTION_HEADERS.iter().any(|h| t == *h)
}

/// Returns true for lines that introduce a new top-level section in the
/// canonical format — used as exit conditions when skipping boilerplate.
fn is_section_boundary(line: &str) -> bool {
    let t = line.trim();
    // H1 markdown: exactly one leading `#` (not `##`)
    let is_h1 = t.starts_with("# ") && !t.starts_with("## ");
    // Special bracket blocks like [Agent Memories]
    let is_bracket = t.starts_with('[') && t.ends_with(']') && t.len() > 2;
    // Run-context XML
    let is_run_ctx = t == "<run-context>" || t.starts_with("<run-context ");
    is_h1 || is_bracket || is_run_ctx
}

fn has_placeholder(line: &str) -> bool {
    let t = line.trim();
    t.contains("{{") && t.contains("}}")
}

enum State {
    Normal,
    InRunContext,
    InBoilerplateSection,
}

fn strip_boilerplate(raw: &str) -> (Vec<String>, usize) {
    let mut kept: Vec<String> = Vec::new();
    let mut stripped: usize = 0;
    let mut state = State::Normal;

    for line in raw.lines() {
        let t = line.trim();
        match state {
            State::Normal => {
                if t == "<run-context>" || t.starts_with("<run-context ") {
                    state = State::InRunContext;
                    stripped += 1;
                } else if is_boilerplate_header(line) {
                    state = State::InBoilerplateSection;
                    stripped += 1;
                } else if has_placeholder(line) {
                    stripped += 1;
                    // Preserve paragraph boundary so text above and below the
                    // stripped placeholder don't merge into the same paragraph.
                    kept.push(String::new());
                } else {
                    kept.push(line.to_string());
                }
            }
            State::InRunContext => {
                stripped += 1;
                if t == "</run-context>" {
                    state = State::Normal;
                }
            }
            State::InBoilerplateSection => {
                if is_section_boundary(line) {
                    if t == "<run-context>" || t.starts_with("<run-context ") {
                        state = State::InRunContext;
                        stripped += 1;
                    } else if is_boilerplate_header(line) {
                        // Another boilerplate section — continue skipping
                        stripped += 1;
                    } else {
                        // User-authored section — keep it and return to Normal
                        state = State::Normal;
                        kept.push(line.to_string());
                    }
                } else {
                    stripped += 1;
                }
            }
        }
    }

    (kept, stripped)
}

// ---------------------------------------------------------------------------
// Content classification
// ---------------------------------------------------------------------------

/// Action verb prefixes that identify imperative lines.
static IMPERATIVE_PREFIXES: &[&str] = &[
    "do ", "do not ", "don't ", "dont ",
    "always ", "never ", "avoid ", "when ",
];

fn is_imperative_line(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    IMPERATIVE_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// Split `lines` into paragraphs (empty-line-separated) and classify each.
/// Paragraphs containing at least one imperative line → special_instructions.
/// All other paragraphs → persona.
fn classify_content(lines: &[String]) -> (String, String) {
    let mut paragraphs: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for line in lines {
        if line.trim().is_empty() {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line.clone());
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }

    let mut persona_parts: Vec<String> = Vec::new();
    let mut instructions_parts: Vec<String> = Vec::new();

    for para in paragraphs {
        let has_imperative = para.iter().any(|l| is_imperative_line(l));
        let block = para.join("\n");
        if has_imperative {
            instructions_parts.push(block);
        } else {
            persona_parts.push(block);
        }
    }

    (persona_parts.join("\n\n"), instructions_parts.join("\n\n"))
}

fn nonempty(s: String) -> Option<String> {
    let t = s.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_empty_prompt() {
        let r = migrate_legacy_system_prompt("");
        assert!(r.persona.is_none());
        assert!(r.special_instructions.is_none());
        assert_eq!(r.stripped_boilerplate_lines, 0);
    }

    #[test]
    fn migrate_whitespace_only_prompt() {
        let r = migrate_legacy_system_prompt("   \n\n  \n");
        assert!(r.persona.is_none());
        assert!(r.special_instructions.is_none());
        assert_eq!(r.stripped_boilerplate_lines, 0);
    }

    #[test]
    fn migrate_boilerplate_only_prompt() {
        let raw = "# Tool Selection: Direct Tools vs. Sub-Agents\n\nPrefer direct tools.\n\n# Memory Management\n\nSave memories with tags.\n\n# Conversation History Recall\n\nUse recall tags.\n";
        let r = migrate_legacy_system_prompt(raw);
        assert!(r.persona.is_none(), "expected no persona, got {:?}", r.persona);
        assert!(r.special_instructions.is_none(), "expected no instructions, got {:?}", r.special_instructions);
        assert!(r.stripped_boilerplate_lines > 0, "expected boilerplate lines stripped");
    }

    #[test]
    fn migrate_boilerplate_plus_persona_prose() {
        let raw = "You are an expert TypeScript developer.\nYou specialize in React and Node.js.\n\n# Memory Management\n\nSave memories.\n";
        let r = migrate_legacy_system_prompt(raw);
        assert!(r.persona.is_some(), "expected persona to be present");
        assert!(r.special_instructions.is_none(), "expected no instructions");
        let persona = r.persona.unwrap();
        assert!(persona.contains("expert TypeScript developer"), "persona: {}", persona);
        assert!(!persona.contains("Memory Management"), "boilerplate leaked into persona");
        assert!(r.stripped_boilerplate_lines > 0);
    }

    #[test]
    fn migrate_boilerplate_plus_persona_and_instructions() {
        let raw = concat!(
            "You are an expert TypeScript developer.\n",
            "You specialize in React and the Launchpad Studio codebase.\n",
            "\n",
            "Always use TypeScript strict mode.\n",
            "Never use the `any` type.\n",
            "Do not commit directly to main.\n",
            "\n",
            "# Memory Management\n",
            "\n",
            "Save memories with tags.\n",
        );
        let r = migrate_legacy_system_prompt(raw);
        assert!(r.persona.is_some(), "expected persona");
        assert!(r.special_instructions.is_some(), "expected instructions");
        let persona = r.persona.as_deref().unwrap();
        assert!(persona.contains("expert TypeScript developer"), "persona: {}", persona);
        let instrs = r.special_instructions.as_deref().unwrap();
        assert!(instrs.contains("strict mode"), "instructions: {}", instrs);
        assert!(instrs.contains("any"), "instructions: {}", instrs);
        assert!(!instrs.contains("Memory Management"), "boilerplate leaked");
    }

    #[test]
    fn migrate_placeholder_tokens_stripped() {
        let raw = concat!(
            "You are a helpful assistant.\n",
            "{{workflow_context}}\n",
            "{{delegate_targets}}\n",
            "{{memory_context}}\n",
            "Do not hallucinate.\n",
        );
        let r = migrate_legacy_system_prompt(raw);
        assert_eq!(r.stripped_boilerplate_lines, 3, "expected 3 placeholder lines stripped");
        assert!(r.persona.is_some(), "expected persona");
        assert!(r.special_instructions.is_some(), "expected instructions");
        let persona = r.persona.as_deref().unwrap();
        assert!(!persona.contains("{{"), "placeholder leaked into persona");
        let instrs = r.special_instructions.as_deref().unwrap();
        assert!(instrs.contains("hallucinate"), "instructions: {}", instrs);
    }

    #[test]
    fn migrate_run_context_block_stripped() {
        let raw = concat!(
            "<run-context>\n",
            "  <cwd>/some/project</cwd>\n",
            "  <os>macos</os>\n",
            "  <date>2026-05-20</date>\n",
            "</run-context>\n",
            "\n",
            "You are a helpful coding agent.\n",
        );
        let r = migrate_legacy_system_prompt(raw);
        assert!(r.stripped_boilerplate_lines >= 5, "run-context block (5 lines) should be stripped");
        assert!(r.persona.is_some(), "expected persona after stripping run-context");
        let persona = r.persona.as_deref().unwrap();
        assert!(persona.contains("helpful coding agent"), "persona: {}", persona);
        assert!(!persona.contains("<run-context>"), "run-context leaked");
    }

    #[test]
    fn migrate_workflows_and_delegates_stripped() {
        let raw = concat!(
            "You are a pipeline orchestration agent.\n",
            "\n",
            "# Workflows\n",
            "\n",
            "The following workflows are available:\n",
            "- **build**: Build pipeline\n",
            "\n",
            "# Delegate Targets\n",
            "\n",
            "The following agents are available:\n",
            "- **qa-agent** — Quality assurance\n",
        );
        let r = migrate_legacy_system_prompt(raw);
        let persona = r.persona.as_deref().unwrap_or("");
        assert!(persona.contains("pipeline orchestration"), "persona: {}", persona);
        assert!(!persona.contains("Workflows"), "Workflows section leaked");
        assert!(!persona.contains("Delegate Targets"), "Delegates section leaked");
        assert!(r.stripped_boilerplate_lines > 0);
    }
}
