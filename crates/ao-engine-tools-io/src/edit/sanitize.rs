//! Helpers for undoing API-level XML-token abbreviation in model output.
//!
//! The Anthropic API automatically replaces certain XML-like tags in assistant
//! output with shorter stand-in tokens so they are not confused with
//! structured API framing. For example, a tag like `<function_results>`
//! becomes a short stand-in token when sent back through the API.
//!
//! When a model references file content containing one of these XML-like tags,
//! its old_string will carry the abbreviated token, causing an exact-match
//! lookup against the original file to fail silently. This module provides
//! the reverse mapping so the Edit tool can recover from that mismatch.

/// Expands known abbreviated XML stand-in tokens back to their full forms.
///
/// Applied as a fallback when exact-match (including quote normalization) has
/// already failed. If the input contains no abbreviations, the returned string
/// equals the input.
pub(super) fn expand_sanitized_tokens(s: &str) -> String {
    // Each pair is (abbreviated_stand_in, full_form). Within the current set
    // no two stand-ins share a prefix, so sequential replacement is correct.
    const EXPANSIONS: &[(&str, &str)] = &[
        ("<fnr>", "<function_results>"),
        ("<n>", "<name>"),
        ("</n>", "</name>"),
        ("<o>", "<output>"),
        ("</o>", "</output>"),
        ("<e>", "<error>"),
        ("</e>", "</error>"),
        ("<s>", "<system>"),
        ("</s>", "</system>"),
        ("<r>", "<result>"),
        ("</r>", "</result>"),
        ("< META_START >", "<META_START>"),
        ("< META_END >", "<META_END>"),
        ("< EOT >", "<EOT>"),
        ("< META >", "<META>"),
        ("< SOS >", "<SOS>"),
        ("\n\nH:", "\n\nHuman:"),
        ("\n\nA:", "\n\nAssistant:"),
    ];
    let mut result = s.to_string();
    for (from, to) in EXPANSIONS {
        if result.contains(from) {
            result = result.replace(from, to);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_function_results_stand_in() {
        let input = "before <fnr> after";
        let got = expand_sanitized_tokens(input);
        assert_eq!(got, "before <function_results> after");
    }

    #[test]
    fn expand_name_tags() {
        let input = "<n>foo</n>";
        let got = expand_sanitized_tokens(input);
        assert_eq!(got, "<name>foo</name>");
    }

    #[test]
    fn expand_no_op_when_no_tokens() {
        let s = "plain text without any xml tokens";
        assert_eq!(expand_sanitized_tokens(s), s);
    }

    #[test]
    fn expand_multiple_tokens_in_one_string() {
        let input = "<fnr><n>val</n>";
        let got = expand_sanitized_tokens(input);
        assert!(got.contains("<function_results>"));
        assert!(got.contains("<name>val</name>"));
    }
}
