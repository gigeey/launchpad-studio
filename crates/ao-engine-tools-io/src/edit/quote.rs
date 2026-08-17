//! Quote-normalisation helpers for the Edit tool.
//!
//! Prose files often contain typographic ("curly") quotes while a model emits
//! straight ASCII quotes. These helpers let Edit match across that mismatch
//! while preserving the file's original typography in the replacement text.

/// Map left/right curly single quotes (U+2018, U+2019) and left/right curly
/// double quotes (U+201C, U+201D) to their straight ASCII equivalents.
/// All other characters pass through unchanged.
///
/// This is a **1-to-1 character mapping**: every input character produces
/// exactly one output character, so char-counts are identical between a string
/// and its normalized form.
pub(super) fn normalize_quotes(s: &str) -> String {
    s.chars()
        .map(|ch| match ch {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            _ => ch,
        })
        .collect()
}

/// Find the substring of `haystack` that matches `needle` after normalizing
/// curly quotes on both sides.
///
/// Returns `Some(actual)` where `actual` is the original-typography slice from
/// `haystack` (preserving any curly quotes that are in the file), or `None`
/// when no match is found even after normalization.
///
/// The byte-offset-to-char-offset conversion is required because curly quotes
/// are 3 bytes each in UTF-8 while their normalized straight-quote equivalents
/// are 1 byte each. Slicing the *original* haystack must use char boundaries.
pub(super) fn find_actual_string(haystack: &str, needle: &str) -> Option<String> {
    // Fast path: exact match — no normalization needed.
    if haystack.contains(needle) {
        return Some(needle.to_string());
    }

    let normalized_haystack = normalize_quotes(haystack);
    let normalized_needle = normalize_quotes(needle);

    // Locate the normalized needle in the normalized haystack (byte offset in
    // the *normalized* string).
    let norm_byte_offset = normalized_haystack.find(normalized_needle.as_str())?;

    // Because normalize_quotes is 1:1 char-count-preserving, the char offset
    // is the same in both the normalized and original strings. Convert the
    // normalized byte offset to a char offset so we can index the original
    // with proper char-boundary safety.
    let char_offset = normalized_haystack[..norm_byte_offset].chars().count();
    let needle_char_len = needle.chars().count();

    let actual: String = haystack
        .chars()
        .skip(char_offset)
        .take(needle_char_len)
        .collect();

    if actual.chars().count() == needle_char_len {
        Some(actual)
    } else {
        None
    }
}

/// Apply the quote-style transformation implied by `original_old → actual_old`
/// onto `new_str` so the replacement text keeps the file's typography.
///
/// When `original_old == actual_old` (no quote-style mismatch), `new_str` is
/// returned unchanged. Otherwise a char→char substitution table is built from
/// the differences and applied to every character in `new_str`.
pub(super) fn preserve_quote_style(original_old: &str, actual_old: &str, new_str: &str) -> String {
    if original_old == actual_old {
        return new_str.to_string();
    }

    // Build a substitution map from chars that differ between the two versions.
    let mut quote_map: std::collections::HashMap<char, char> = std::collections::HashMap::new();
    for (orig_ch, actual_ch) in original_old.chars().zip(actual_old.chars()) {
        if orig_ch != actual_ch {
            quote_map.insert(orig_ch, actual_ch);
        }
    }

    new_str
        .chars()
        .map(|ch| quote_map.get(&ch).copied().unwrap_or(ch))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── normalize_quotes ──────────────────────────────────────────────────────

    #[test]
    fn normalize_quotes_passes_ascii_through() {
        let s = "hello \"world\" it's fine";
        assert_eq!(normalize_quotes(s), s);
    }

    #[test]
    fn normalize_quotes_converts_all_four_curly_variants() {
        assert_eq!(normalize_quotes("\u{2018}"), "'"); // left single curly
        assert_eq!(normalize_quotes("\u{2019}"), "'"); // right single curly
        assert_eq!(normalize_quotes("\u{201C}"), "\""); // left double curly
        assert_eq!(normalize_quotes("\u{201D}"), "\""); // right double curly
    }

    // ── find_actual_string ────────────────────────────────────────────────────

    #[test]
    fn find_actual_string_exact_match_returns_needle() {
        let result = find_actual_string("hello world", "world");
        assert_eq!(result, Some("world".to_string()));
    }

    #[test]
    fn find_actual_string_curly_file_straight_needle_returns_original_typography() {
        // File has curly apostrophe; model sends straight apostrophe.
        let haystack = "don\u{2019}t forget";
        let needle = "don't forget";
        let result = find_actual_string(haystack, needle);
        // Must return the original curly-quote substring, not the needle.
        assert_eq!(result, Some("don\u{2019}t forget".to_string()));
    }

    #[test]
    fn find_actual_string_not_found_returns_none() {
        assert_eq!(find_actual_string("hello world", "goodbye"), None);
    }

    #[test]
    fn find_actual_string_multibyte_span() {
        // Needle spans multi-byte curly-quote code points in original haystack.
        let haystack = "say \u{201C}hello\u{201D} now";
        let needle = "\"hello\"";
        let result = find_actual_string(haystack, needle);
        assert_eq!(result, Some("\u{201C}hello\u{201D}".to_string()));
    }

    // ── preserve_quote_style ─────────────────────────────────────────────────

    #[test]
    fn preserve_quote_style_no_change_when_strings_identical() {
        let result = preserve_quote_style("don't", "don't", "won't");
        assert_eq!(result, "won't");
    }

    #[test]
    fn preserve_quote_style_applies_straight_to_curly_correction() {
        // original_old used straight apostrophe; actual_old (file) used curly.
        // new_str also uses straight apostrophe → should become curly.
        let result = preserve_quote_style("don't", "don\u{2019}t", "won't");
        assert_eq!(result, "won\u{2019}t");
    }

    #[test]
    fn preserve_quote_style_applies_curly_to_straight_correction() {
        // original_old used curly; actual_old (file) used straight.
        let result = preserve_quote_style("don\u{2019}t", "don't", "won\u{2019}t");
        assert_eq!(result, "won't");
    }
}
