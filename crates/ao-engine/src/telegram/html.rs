//! Converts the CommonMark-ish text an agent produces into the small HTML
//! subset Telegram's `sendMessage` accepts under `parse_mode: "HTML"`.
//!
//! Telegram has no CommonMark parser: sent as plain text, `**bold**`,
//! `# heading`, and friends show up as literal punctuation on the phone.
//! Telegram's HTML dialect is deliberately tiny — only
//! `<b> <i> <u> <s> <code> <pre> <a href> <span class="tg-spoiler">
//! <blockquote>` are recognized — so constructs with no equivalent (headings,
//! tables, ...) degrade to a readable plain-text approximation instead of
//! being dropped or leaking raw markdown syntax.
//!
//! There is no CommonMark crate in this workspace, so this is a small
//! hand-written converter rather than a full spec-compliant parser. It
//! covers the constructs an agent reply actually uses: emphasis, inline
//! code, links, fenced code, headings, bullets, blockquotes, and tables.
//! Exotic CommonMark corners (nested brackets in link text, mismatched
//! emphasis-run lengths like `***text***`) are not specifically handled;
//! unmatched delimiters simply fall through as literal, escaped text.

/// Converts `source` into Telegram's HTML subset.
pub(super) fn markdown_to_telegram_html(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim_start();

        if let Some((fence_char, fence_len, lang)) = fence_open(trimmed) {
            let (rendered, next) = render_fence(&lines, i, fence_char, fence_len, lang);
            out.push(rendered);
            i = next;
            continue;
        }

        if is_table_row(lines[i]) && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let (rendered, next) = render_table(&lines, i);
            out.push(rendered);
            i = next;
            continue;
        }

        if trimmed.starts_with('>') {
            let (rendered, next) = render_blockquote(&lines, i);
            out.push(rendered);
            i = next;
            continue;
        }

        out.push(render_plain_line(lines[i]));
        i += 1;
    }

    out.join("\n")
}

// ---------------------------------------------------------------------
// Block level
// ---------------------------------------------------------------------

/// Renders a single non-fence, non-table, non-blockquote line: a heading, a
/// bullet, or a plain paragraph (which also covers ordered-list items —
/// `1. item` has no special characters of its own, so inline rendering the
/// whole line already produces `1. item` verbatim with `item`'s own markdown
/// still processed).
fn render_plain_line(line: &str) -> String {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let content = &line[indent_len..];

    if let Some(text) = heading_text(content) {
        return format!("{indent}<b>{}</b>", render_inline(text));
    }
    if let Some(text) = bullet_text(content) {
        return format!("{indent}\u{2022} {}", render_inline(text));
    }
    render_inline(line)
}

/// Matches `#`..`######` followed by a space, returning the heading text.
/// Telegram has no heading tag, so callers bold the text instead.
fn heading_text(s: &str) -> Option<&str> {
    let hashes = s.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &s[hashes..];
    let rest = rest.strip_prefix(' ')?;
    Some(rest.trim_end())
}

/// Matches a `-` or `*` bullet marker followed by a space. Requires the
/// literal marker char be followed immediately by whitespace, which also
/// keeps `**bold text**` (no space after the first `*`) from being
/// misdetected as a bullet.
fn bullet_text(s: &str) -> Option<&str> {
    let mut chars = s.chars();
    match chars.next() {
        Some('-') | Some('*') => {}
        _ => return None,
    }
    let rest = &s[1..];
    let rest = rest.strip_prefix(' ')?;
    Some(rest.trim_end())
}

/// Matches a fenced-code opening line (` ``` ` or `~~~`, 3+ chars), returning
/// the fence character, its run length, and the trimmed info-string
/// (typically a language name, possibly empty).
fn fence_open(trimmed: &str) -> Option<(char, usize, &str)> {
    let c = trimmed.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|&x| x == c).count();
    if run < 3 {
        return None;
    }
    Some((c, run, trimmed[run..].trim()))
}

fn is_fence_close(trimmed: &str, fence_char: char, fence_len: usize) -> bool {
    let run = trimmed.chars().take_while(|&x| x == fence_char).count();
    run >= fence_len && trimmed.chars().skip(run).all(char::is_whitespace)
}

/// Renders a fenced code block starting at `lines[start_idx]` through its
/// closing fence (or end of input, if the fence is never closed — treating
/// the remainder as code is friendlier than dumping the opening fence as
/// literal text). Code content is never inline-rendered — only escaped.
fn render_fence(
    lines: &[&str],
    start_idx: usize,
    fence_char: char,
    fence_len: usize,
    lang: &str,
) -> (String, usize) {
    let mut i = start_idx + 1;
    let mut code_lines: Vec<&str> = Vec::new();
    while i < lines.len() {
        if is_fence_close(lines[i].trim(), fence_char, fence_len) {
            i += 1;
            break;
        }
        code_lines.push(lines[i]);
        i += 1;
    }

    let escaped = escape_html(&code_lines.join("\n"));
    let rendered = if lang.is_empty() {
        format!("<pre>{escaped}</pre>")
    } else {
        format!("<pre><code class=\"language-{}\">{escaped}</code></pre>", escape_html(lang))
    };
    (rendered, i)
}

fn is_table_row(line: &str) -> bool {
    !line.trim().is_empty() && line.contains('|')
}

/// A table separator row: cells made up only of `-` with optional leading/
/// trailing `:` (alignment markers), e.g. `--- | :--- | ---:`.
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.contains('-') {
        return false;
    }
    let cells: Vec<&str> = trimmed.trim_matches('|').split('|').map(str::trim).collect();
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let inner = cell.trim_start_matches(':').trim_end_matches(':');
            !inner.is_empty() && inner.chars().all(|c| c == '-')
        })
}

fn split_table_row(line: &str) -> Vec<String> {
    line.trim().trim_matches('|').split('|').map(|c| c.trim().to_string()).collect()
}

/// Renders a markdown table (header + separator + rows) starting at
/// `lines[start_idx]` as a monospace, pipe-free `<pre>` block — Telegram has
/// no table tag, and dumping the raw `|`-delimited syntax as text reads as
/// noise rather than a table. Column widths (and the resulting padding) are
/// computed on the raw cell text, then the whole padded block is escaped
/// once, so escaping never throws off visual alignment.
fn render_table(lines: &[&str], start_idx: usize) -> (String, usize) {
    let mut rows = vec![split_table_row(lines[start_idx])];
    let mut i = start_idx + 2; // skip the header and the separator row
    while i < lines.len() && is_table_row(lines[i]) && !is_table_separator(lines[i]) {
        rows.push(split_table_row(lines[i]));
        i += 1;
    }

    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in &rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(cell.chars().count());
        }
    }

    let rendered_rows: Vec<String> = rows
        .iter()
        .map(|row| {
            (0..cols)
                .map(|idx| {
                    let cell = row.get(idx).map(String::as_str).unwrap_or("");
                    let pad = widths[idx].saturating_sub(cell.chars().count());
                    format!("{cell}{}", " ".repeat(pad))
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect();

    (format!("<pre>{}</pre>", escape_html(&rendered_rows.join("\n"))), i)
}

/// Renders consecutive `>`-prefixed lines starting at `lines[start_idx]` as
/// one `<blockquote>` block.
fn render_blockquote(lines: &[&str], start_idx: usize) -> (String, usize) {
    let mut rendered_lines = Vec::new();
    let mut i = start_idx;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        let Some(content) = trimmed.strip_prefix('>') else {
            break;
        };
        let content = content.strip_prefix(' ').unwrap_or(content);
        rendered_lines.push(render_inline(content));
        i += 1;
    }
    (format!("<blockquote>{}</blockquote>", rendered_lines.join("\n")), i)
}

// ---------------------------------------------------------------------
// Inline level
// ---------------------------------------------------------------------

/// Renders inline markdown (emphasis, code spans, links) within a single
/// span of text. Text outside any recognized construct is escaped and
/// passed through as literal characters.
fn render_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    render_inline_chars(&chars)
}

fn render_inline_chars(chars: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c == '\\' && i + 1 < chars.len() && is_escapable_md_char(chars[i + 1]) {
            escape_html_char_into(chars[i + 1], &mut out);
            i += 2;
            continue;
        }

        if c == '`' {
            let run = run_length(chars, i, '`');
            if let Some(close) = find_backtick_close(chars, i + run, run) {
                let content: String = chars[i + run..close].iter().collect();
                out.push_str("<code>");
                out.push_str(&escape_html(&content));
                out.push_str("</code>");
                i = close + run;
                continue;
            }
            out.push_str(&"`".repeat(run));
            i += run;
            continue;
        }

        if c == '[' {
            if let Some((text_end, url, consumed_end)) = try_parse_link(chars, i) {
                let inner = render_inline_chars(&chars[i + 1..text_end]);
                out.push_str("<a href=\"");
                out.push_str(&escape_html_attr(&url));
                out.push_str("\">");
                out.push_str(&inner);
                out.push_str("</a>");
                i = consumed_end;
                continue;
            }
            escape_html_char_into('[', &mut out);
            i += 1;
            continue;
        }

        if (c == '*' || c == '_') && i + 1 < chars.len() && chars[i + 1] == c {
            if let Some(close) = find_closing_run(chars, i + 2, c, 2) {
                let inner = render_inline_chars(&chars[i + 2..close]);
                out.push_str("<b>");
                out.push_str(&inner);
                out.push_str("</b>");
                i = close + 2;
                continue;
            }
        }

        if c == '~' && i + 1 < chars.len() && chars[i + 1] == '~' {
            if let Some(close) = find_closing_run(chars, i + 2, '~', 2) {
                let inner = render_inline_chars(&chars[i + 2..close]);
                out.push_str("<s>");
                out.push_str(&inner);
                out.push_str("</s>");
                i = close + 2;
                continue;
            }
        }

        if c == '*' || c == '_' {
            if let Some(close) = find_closing_run(chars, i + 1, c, 1) {
                let inner = render_inline_chars(&chars[i + 1..close]);
                out.push_str("<i>");
                out.push_str(&inner);
                out.push_str("</i>");
                i = close + 1;
                continue;
            }
        }

        escape_html_char_into(c, &mut out);
        i += 1;
    }

    out
}

fn run_length(chars: &[char], start: usize, ch: char) -> usize {
    let mut n = 0;
    while start + n < chars.len() && chars[start + n] == ch {
        n += 1;
    }
    n
}

/// Finds a backtick run of exactly `run_len` starting at or after `from`.
/// A run of a different length is skipped whole rather than partially
/// matched, since CommonMark code spans require exact delimiter-length
/// symmetry.
fn find_backtick_close(chars: &[char], from: usize, run_len: usize) -> Option<usize> {
    let mut pos = from;
    while pos < chars.len() {
        if chars[pos] == '`' {
            let r = run_length(chars, pos, '`');
            if r == run_len {
                return Some(pos);
            }
            pos += r;
        } else {
            pos += 1;
        }
    }
    None
}

/// Finds a closing run of `delim` repeated `run_len` times, starting at or
/// after `from`. Inline code spans encountered along the way are skipped
/// whole so a delimiter character inside `` `code` `` never closes emphasis
/// opened outside it; a delimiter run of the wrong length is likewise
/// skipped whole rather than partially consumed.
fn find_closing_run(chars: &[char], from: usize, delim: char, run_len: usize) -> Option<usize> {
    let mut pos = from;
    while pos < chars.len() {
        if chars[pos] == '`' {
            let code_run = run_length(chars, pos, '`');
            match find_backtick_close(chars, pos + code_run, code_run) {
                Some(close) => pos = close + code_run,
                None => pos += code_run,
            }
            continue;
        }
        if chars[pos] == delim {
            let run = run_length(chars, pos, delim);
            if run == run_len {
                return Some(pos);
            }
            pos += run;
            continue;
        }
        pos += 1;
    }
    None
}

/// Parses `[text](url)` starting at `chars[start] == '['`. No nested-bracket
/// support in the text span (the first `]` ends it) — adequate for the
/// single-level links an agent reply actually produces.
fn try_parse_link(chars: &[char], start: usize) -> Option<(usize, String, usize)> {
    let mut i = start + 1;
    while i < chars.len() && chars[i] != ']' {
        i += 1;
    }
    if i >= chars.len() || i + 1 >= chars.len() || chars[i + 1] != '(' {
        return None;
    }
    let text_end = i;
    let mut j = i + 2;
    while j < chars.len() && chars[j] != ')' {
        j += 1;
    }
    if j >= chars.len() {
        return None;
    }
    let url: String = chars[i + 2..j].iter().collect();
    Some((text_end, url, j + 1))
}

fn is_escapable_md_char(c: char) -> bool {
    matches!(
        c,
        '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.' | '!' | '~' | '>' | '|'
    )
}

fn escape_html_char_into(c: char, out: &mut String) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        _ => out.push(c),
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        escape_html_char_into(c, &mut out);
    }
    out
}

fn escape_html_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '"' {
            out.push_str("&quot;");
        } else {
            escape_html_char_into(c, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_bold_with_double_asterisk_and_double_underscore() {
        assert_eq!(markdown_to_telegram_html("**bold**"), "<b>bold</b>");
        assert_eq!(markdown_to_telegram_html("__bold__"), "<b>bold</b>");
    }

    #[test]
    fn renders_italic_with_single_asterisk_and_single_underscore() {
        assert_eq!(markdown_to_telegram_html("*italic*"), "<i>italic</i>");
        assert_eq!(markdown_to_telegram_html("_italic_"), "<i>italic</i>");
    }

    #[test]
    fn renders_strikethrough() {
        assert_eq!(markdown_to_telegram_html("~~gone~~"), "<s>gone</s>");
    }

    #[test]
    fn renders_inline_code_without_interpreting_its_contents() {
        assert_eq!(
            markdown_to_telegram_html("`a * b`"),
            "<code>a * b</code>"
        );
    }

    #[test]
    fn renders_link() {
        assert_eq!(
            markdown_to_telegram_html("[docs](https://example.com/x?y=1)"),
            "<a href=\"https://example.com/x?y=1\">docs</a>"
        );
    }

    #[test]
    fn renders_nested_emphasis_inside_a_link_and_bold() {
        assert_eq!(
            markdown_to_telegram_html("**bold with [a link](https://x.test) inside**"),
            "<b>bold with <a href=\"https://x.test\">a link</a> inside</b>"
        );
    }

    #[test]
    fn renders_fenced_code_block_without_language() {
        let input = "```\nlet x = 1;\nlet y = 2;\n```";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<pre>let x = 1;\nlet y = 2;</pre>"
        );
    }

    #[test]
    fn renders_fenced_code_block_with_language() {
        let input = "```rust\nfn main() {}\n```";
        assert_eq!(
            markdown_to_telegram_html(input),
            "<pre><code class=\"language-rust\">fn main() {}</code></pre>"
        );
    }

    #[test]
    fn escapes_reserved_html_characters_in_prose() {
        assert_eq!(
            markdown_to_telegram_html("a < b & b > c"),
            "a &lt; b &amp; b &gt; c"
        );
    }

    #[test]
    fn escapes_reserved_characters_inside_a_code_span() {
        assert_eq!(
            markdown_to_telegram_html("`<script>&`"),
            "<code>&lt;script&gt;&amp;</code>"
        );
    }

    #[test]
    fn escapes_ampersand_in_a_link_url() {
        assert_eq!(
            markdown_to_telegram_html("[x](https://example.com?a=1&b=2)"),
            "<a href=\"https://example.com?a=1&amp;b=2\">x</a>"
        );
    }

    #[test]
    fn degrades_headings_of_any_level_to_bold_text() {
        assert_eq!(markdown_to_telegram_html("# Title"), "<b>Title</b>");
        assert_eq!(markdown_to_telegram_html("### Sub"), "<b>Sub</b>");
    }

    #[test]
    fn degrades_unordered_bullets_to_a_bullet_glyph() {
        assert_eq!(
            markdown_to_telegram_html("- first\n* second"),
            "\u{2022} first\n\u{2022} second"
        );
    }

    #[test]
    fn keeps_ordered_list_markers_as_plain_text() {
        assert_eq!(
            markdown_to_telegram_html("1. **first**\n2. second"),
            "1. <b>first</b>\n2. second"
        );
    }

    #[test]
    fn degrades_a_table_to_a_pipe_free_monospace_block() {
        let input = "| Name | Age |\n| --- | --- |\n| Ada | 30 |\n| Grace | 40 |";
        let rendered = markdown_to_telegram_html(input);
        assert!(rendered.starts_with("<pre>") && rendered.ends_with("</pre>"));
        assert!(!rendered.contains('|'), "table output must not leak raw pipes: {rendered}");
        assert!(rendered.contains("Name") && rendered.contains("Ada") && rendered.contains("Grace"));
    }

    #[test]
    fn renders_blockquote() {
        assert_eq!(
            markdown_to_telegram_html("> quoted line one\n> quoted line two"),
            "<blockquote>quoted line one\nquoted line two</blockquote>"
        );
    }

    #[test]
    fn leaves_an_unmatched_delimiter_as_literal_text() {
        assert_eq!(markdown_to_telegram_html("2 * 3 = 6"), "2 * 3 = 6");
    }
}
