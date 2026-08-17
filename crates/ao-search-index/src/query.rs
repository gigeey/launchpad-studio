/// Turn free-text user input into a safe SQLite FTS5 `MATCH` expression.
///
/// FTS5 query syntax treats bareword input as an expression language
/// (`AND` / `OR` / `NOT`, `-prefix`, `^column`, unbalanced quotes, etc.), so
/// passing a caller's raw text straight into `MATCH` both risks a syntax
/// error and lets query text accidentally exclude/require terms the caller
/// never intended as operators. Splitting on non-alphanumeric boundaries
/// (which drops any `"` along with every other non-word character) and
/// re-quoting every resulting token as a literal phrase neutralizes all of
/// that: each token can only ever mean "this literal word," never an
/// operator. Tokens are OR'd together so a query matches any entry
/// containing at least one term, with `bm25` ranking rewarding entries that
/// match more of them.
///
/// Returns `None` when `input` has no indexable tokens (empty/whitespace/
/// punctuation-only), signaling the caller should skip the query entirely.
pub(crate) fn build_match_expression(input: &str) -> Option<String> {
    let tokens: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{tok}\""))
        .collect();

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_none() {
        assert_eq!(build_match_expression(""), None);
        assert_eq!(build_match_expression("   "), None);
        assert_eq!(build_match_expression("---"), None);
    }

    #[test]
    fn single_word_wraps_as_phrase() {
        assert_eq!(build_match_expression("hello"), Some("\"hello\"".to_string()));
    }

    #[test]
    fn multiple_words_join_with_or() {
        assert_eq!(
            build_match_expression("hello world"),
            Some("\"hello\" OR \"world\"".to_string())
        );
    }

    #[test]
    fn fts5_operators_are_neutralized_as_literal_tokens() {
        // "AND"/"OR"/"NOT" and leading "-" are FTS5 syntax when bare; once
        // split into tokens and quoted, they can only match the literal word.
        assert_eq!(
            build_match_expression("AND OR NOT"),
            Some("\"AND\" OR \"OR\" OR \"NOT\"".to_string())
        );
    }

    #[test]
    fn embedded_double_quotes_are_dropped_as_delimiters() {
        assert_eq!(
            build_match_expression("he said \"hi\""),
            Some("\"he\" OR \"said\" OR \"hi\"".to_string())
        );
    }
}
