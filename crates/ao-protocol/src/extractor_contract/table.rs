//! Tier 2 tabular extraction: reading rows out of tabular markup — an HTML
//! `<table>` or a markdown pipe table — embedded as a literal string inside
//! an otherwise-structured JSON payload. Many tool responses carry their
//! real payload this way: a JSON envelope (`{metadata, title, url, text}`)
//! whose `text` field is itself a rendered snippet of markup, not further
//! structured data. [`ExtractorKind::Table`](super::ExtractorKind::Table)
//! is the selector mechanism that reads it; this module is where finding
//! and parsing that markup actually happens, kept separate from `mod.rs`
//! purely for size.
//!
//! [`discover_tabular_field`] is the authoring-time entry point: given a
//! whole payload, it finds the one string field (if there is exactly one)
//! that contains exactly one recognizable table, and returns its rows
//! already shaped as JSON objects keyed by normalized header cells — with
//! any blank/placeholder row filtered out first by
//! [`filter_blank_identity_rows`]. [`find_tables_in_text`], [`row_to_value`],
//! and [`filter_blank_identity_rows`] are the same primitives `mod.rs`'s
//! `select_items` calls again on every later poll, against a frozen
//! `field_path`/`columns`/`identity_columns` triple, so a poll's binding
//! step is always evaluating the identical parse (blank rows included) this
//! module authored the plan from.
//!
//! No table parsed here is ever assumed well-formed relative to any other:
//! [`discover_tabular_field`] refuses to guess between two candidate tables
//! (or two candidate fields) rather than picking one arbitrarily — see its
//! own doc.

use std::collections::HashMap;

use serde_json::Value;

/// One recognizable table [`discover_tabular_field`] found: the payload's
/// own dotted-path to the string field that held it (the same dialect
/// [`super::resolve_json_path`] resolves — empty for the payload root
/// itself), its header cells already normalized into stable, duplicate-free
/// field keys (column order preserved), and its data rows already shaped as
/// JSON objects keyed by those same field keys.
pub struct TableField {
    pub field_path: String,
    pub columns: Vec<String>,
    pub rows: Vec<Value>,
}

/// One table as parsed off raw markup, before header normalization: cell
/// text only, never yet paired with a column key. Kept crate-private —
/// nothing outside `extractor_contract` needs a table's raw shape, only
/// [`TableField`]'s already-normalized one. Fields are `pub(crate)` (not
/// private) because `mod.rs`'s `select_items` — an ancestor module, which
/// cannot see a descendant module's private items — reads them directly
/// when re-parsing a frozen `Table` selector's `field_path` on every poll.
pub(crate) struct ParsedTable {
    pub(crate) header: Vec<String>,
    pub(crate) rows: Vec<Vec<String>>,
}

const MAX_DISCOVERY_PATHS: usize = 200;

/// Walks `payload` (bounded to `max_depth` levels of object/array nesting,
/// mirroring [`super::enumerate_paths`]'s own bound) looking for exactly one
/// string-valued field that contains exactly one recognizable table. Checks
/// the payload root itself too, in case it's a bare string rather than an
/// object.
///
/// Deliberately refuses to guess: if the payload contains zero tables, or
/// more than one — whether spread across different string fields or piled
/// up inside a single one — this returns `None` rather than picking a
/// candidate arbitrarily, exactly the same "don't guess" stance
/// `ao_engine::agent_watch`'s own row-shaped-array selection takes for Tier
/// 1. The caller's only recourse on `None` is to keep falling back to a
/// model read for this payload.
///
/// `identity_columns` (already normalized via [`normalize_column_key`], the
/// caller's job — this module has no notion of a `WatchContract`) is handed
/// to [`filter_blank_identity_rows`] before `rows` is returned, so a table's
/// own blank/placeholder rows never reach the caller as if they were real
/// data — see that function's doc for the exact rule.
pub fn discover_tabular_field(payload: &Value, max_depth: usize, identity_columns: &[String]) -> Option<TableField> {
    let mut candidates: Vec<(String, ParsedTable)> = Vec::new();

    if let Value::String(s) = payload {
        candidates.extend(find_tables_in_text(s).into_iter().map(|t| (String::new(), t)));
    }
    for info in super::enumerate_paths(payload, max_depth, MAX_DISCOVERY_PATHS) {
        if info.value_type != "string" {
            continue;
        }
        if let Some(Value::String(s)) = super::resolve_json_path(payload, &info.path) {
            candidates.extend(find_tables_in_text(s).into_iter().map(|t| (info.path.clone(), t)));
        }
    }

    if candidates.len() != 1 {
        return None;
    }
    let (field_path, table) = candidates.into_iter().next().expect("checked len == 1 above");
    let columns = normalize_header(&table.header);
    if columns.is_empty() {
        return None;
    }
    let rows: Vec<Value> = table.rows.iter().map(|row| row_to_value(&columns, row)).collect();
    let rows = filter_blank_identity_rows(rows, identity_columns);
    Some(TableField { field_path, columns, rows })
}

/// Drops every row in `rows` (already shaped as [`row_to_value`] objects,
/// keyed by normalized column) whose `identity_columns` are ALL blank per
/// [`is_blank_cell`] — a header-adjacent template/placeholder row that
/// carries no data under any of the table's own identity columns counts as
/// blank even when some other, non-identity column (a stray note, leftover
/// formatting) has content: a row with no identity has no stable identity to
/// mint an entity from, so trailing content elsewhere doesn't rescue it. A
/// row where none of `identity_columns` is even present as a key is treated
/// the same as one where they're present but empty.
///
/// `identity_columns` empty is a deliberate no-op (every row is kept,
/// unfiltered) rather than dropping everything — the caller not knowing its
/// own identity fields is not evidence every row is blank, and failing open
/// here is what keeps an old, already-persisted plan (frozen before this
/// field existed) behaving exactly as it did before.
pub(crate) fn filter_blank_identity_rows(rows: Vec<Value>, identity_columns: &[String]) -> Vec<Value> {
    if identity_columns.is_empty() {
        return rows;
    }
    rows.into_iter()
        .filter(|row| {
            let Some(obj) = row.as_object() else { return true };
            identity_columns.iter().any(|column| {
                obj.get(column).is_some_and(|value| match value {
                    Value::String(s) => !is_blank_cell(s),
                    Value::Null => false,
                    _ => true,
                })
            })
        })
        .collect()
}

/// True when `s` renders as nothing: empty once trimmed, or made up
/// entirely of zero-width Unicode filler — U+200B ZERO WIDTH SPACE, U+200C
/// ZERO WIDTH NON-JOINER, U+200D ZERO WIDTH JOINER, U+FEFF ZERO WIDTH NO-BREAK
/// SPACE (a stray BOM) — none of which Rust's own `char::is_whitespace`
/// treats as whitespace (they sit outside Unicode's `White_Space` property),
/// so a cell containing only one of these would otherwise read as
/// "non-empty" text. [`clean_cell_text`] already collapses ordinary
/// whitespace and decodes `&nbsp;`/strips `<br>`/empty `<p></p>` down to
/// plain spaces or nothing before a cell ever reaches this check, so this
/// only needs to cover the zero-width gap that whitespace collapsing alone
/// doesn't.
fn is_blank_cell(s: &str) -> bool {
    s.chars().all(|c| c.is_whitespace() || matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}'))
}

/// Every independently recognizable table inside `text` — HTML and markdown
/// candidates combined, in no particular relative order. Used both by
/// [`discover_tabular_field`] (to count candidates across the whole
/// payload) and by `mod.rs`'s `select_items` (to re-parse a frozen plan's
/// `field_path` on a later poll) — the same function on both ends is what
/// makes a poll's binding step a faithful replay of what authoring saw.
pub(crate) fn find_tables_in_text(text: &str) -> Vec<ParsedTable> {
    let mut tables = find_html_tables(text);
    tables.extend(find_markdown_tables(text));
    tables
}

/// One data row's cells, positionally paired with `columns` (column `i`'s
/// key gets cell `i`'s text; a short row pads missing trailing cells with
/// an empty string, a long row drops extras) — the same shape both
/// authoring and every later poll's binding step produce for a `Table`
/// selector, so a row's identity/material fields always land under the
/// exact keys the frozen plan (and the `WatchContract` it's bound to)
/// expect.
pub(crate) fn row_to_value(columns: &[String], cells: &[String]) -> Value {
    let mut obj = serde_json::Map::with_capacity(columns.len());
    for (i, column) in columns.iter().enumerate() {
        obj.insert(column.clone(), Value::String(cells.get(i).cloned().unwrap_or_default()));
    }
    Value::Object(obj)
}

/// Normalizes one header cell into a stable field key: trimmed, lowercased,
/// every run of non-alphanumeric characters collapsed to a single
/// underscore, with no leading/trailing underscore. `"First Name"`,
/// `"first_name"`, and `"First-Name"` all normalize to `"first_name"` — the
/// same normalization a Tier 2 replay-and-diff comparison applies to a
/// `WatchContract`'s own declared field names, so minor formatting
/// differences between a table's header text and a contract's field naming
/// don't spuriously fail that comparison.
pub fn normalize_column_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_underscore = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_underscore && !out.is_empty() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            pending_underscore = false;
        } else {
            pending_underscore = true;
        }
    }
    out
}

/// Normalizes a whole header row into a duplicate-free list of field keys,
/// in column order, via [`normalize_column_key`]. A collision — two header
/// cells normalizing to the same key — is disambiguated by appending `_2`,
/// `_3`, ... to each later occurrence, so a malformed or repeated header
/// column is never silently dropped from the resulting row objects.
fn normalize_header(header: &[String]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    header
        .iter()
        .map(|cell| {
            let base = normalize_column_key(cell);
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                base
            } else {
                format!("{base}_{count}")
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// HTML `<table>` parsing
// ---------------------------------------------------------------------------

/// Case-insensitive, byte-exact substring search starting at byte offset
/// `from`. Deliberately not a `to_lowercase()`-then-compare: lowering a copy
/// of arbitrary cell content first could shift byte offsets out from under
/// the original string for a handful of Unicode code points whose
/// lowercase form is longer than their uppercase one, which would silently
/// corrupt every slice taken using those offsets.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() || from > hb.len() || nb.len() > hb.len() - from {
        return None;
    }
    (from..=hb.len() - nb.len()).find(|&i| hb[i..i + nb.len()].eq_ignore_ascii_case(nb))
}

/// Byte offset of the next `<tag` occurrence at or after `from`, skipping
/// any match that is really a longer tag name's prefix (e.g. `<th` inside
/// `<thead`) — the character immediately following the matched name must
/// not itself continue an identifier, or this keeps searching past it.
fn find_tag_open(text: &str, tag: &str, from: usize) -> Option<usize> {
    let needle = format!("<{tag}");
    let mut pos = from;
    loop {
        let start = find_ci(text, &needle, pos)?;
        let after_name = start + needle.len();
        if text[after_name..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
            pos = start + 1;
            continue;
        }
        return Some(start);
    }
}

/// The next `<tag ...> ... </tag>` block in `text` at or after `from`: the
/// tag's own open-tag start (for callers that need to compare two
/// candidate tags' document order — see [`find_cells`]), its inner content,
/// and the byte offset just past its closing tag. `None` once no further
/// opening tag is found, or one has no matching close — a block missing its
/// close is dropped rather than treated as extending to end-of-string,
/// since a table this malformed has nothing reliable left to parse anyway.
fn next_tag_block<'a>(text: &'a str, tag: &str, from: usize) -> Option<(usize, &'a str, usize)> {
    let open_start = find_tag_open(text, tag, from)?;
    let after_name = open_start + tag.len() + 1;
    let open_tag_end = after_name + text[after_name..].find('>')? + 1;
    let close_needle = format!("</{tag}");
    let close_start = find_ci(text, &close_needle, open_tag_end)?;
    let close_tag_end = close_start + text[close_start..].find('>')? + 1;
    Some((open_start, &text[open_tag_end..close_start], close_tag_end))
}

fn find_tag_blocks<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    let mut pos = 0usize;
    while let Some((_, inner, next_pos)) = next_tag_block(text, tag, pos) {
        blocks.push(inner);
        pos = next_pos;
    }
    blocks
}

fn find_html_tables(text: &str) -> Vec<ParsedTable> {
    find_tag_blocks(text, "table").into_iter().filter_map(parse_html_table_body).collect()
}

/// Parses one `<table>` element's inner content: every `<tr>` block becomes
/// a row, and — per row — every `<td>`/`<th>` cell in document order becomes
/// one cell's text (see [`find_cells`]). The FIRST row is always the
/// header, whichever cell tag it happens to use: a `<th>`-cell first row is
/// the ordinary case, and a table with no `<th>` at all falls back to
/// treating its first plain `<tr>` as the header — both are the exact same
/// code path here, since [`find_cells`] itself doesn't care which tag a
/// cell uses.
fn parse_html_table_body(inner: &str) -> Option<ParsedTable> {
    let mut rows: Vec<Vec<String>> = find_tag_blocks(inner, "tr").into_iter().map(find_cells).filter(|r| !r.is_empty()).collect();
    if rows.is_empty() {
        return None;
    }
    let header = rows.remove(0);
    if header.is_empty() {
        return None;
    }
    Some(ParsedTable { header, rows })
}

/// Every `<td>`/`<th>` cell inside one `<tr>` block's inner content, in the
/// document order they actually appear (not two separate td-then-th
/// passes) — so a row that mixes both tags still comes out in reading
/// order.
fn find_cells(tr_inner: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut pos = 0usize;
    loop {
        let td = next_tag_block(tr_inner, "td", pos);
        let th = next_tag_block(tr_inner, "th", pos);
        let chosen = match (td, th) {
            (None, None) => break,
            (Some(t), None) => t,
            (None, Some(h)) => h,
            (Some(t), Some(h)) => {
                if t.0 <= h.0 {
                    t
                } else {
                    h
                }
            }
        };
        cells.push(clean_cell_text(chosen.1));
        pos = chosen.2;
    }
    cells
}

/// Strips any nested tags (`<td><b>Peter</b></td>` -> `Peter`), decodes the
/// handful of HTML entities real-world table cells actually use, and
/// collapses surrounding/internal whitespace (pretty-printed markup often
/// wraps cell content across lines) down to single spaces, trimmed.
fn clean_cell_text(raw: &str) -> String {
    let stripped = strip_tags(raw);
    let decoded = decode_html_entities(&stripped);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            _ => out.push(ch),
        }
    }
    out
}

/// `&amp;` is decoded last, so a literal `&amp;lt;` in the source (meaning
/// the two characters `&lt;`) decodes to exactly that instead of cascading
/// on into `<` — a single, non-recursive pass over a fixed replacement
/// list, not a general entity decoder.
fn decode_html_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// Markdown pipe-table parsing
// ---------------------------------------------------------------------------

/// Scans `text` line by line for `| header | cells |` rows immediately
/// followed by a `|---|---|`-style separator row (dashes/colons only, one
/// cell per header cell) — a markdown table's mandatory second line — and
/// collects every subsequent pipe-shaped line as a data row until the first
/// line that isn't one.
fn find_markdown_tables(text: &str) -> Vec<ParsedTable> {
    let lines: Vec<&str> = text.lines().collect();
    let mut tables = Vec::new();
    let mut i = 0usize;
    while i + 1 < lines.len() {
        if !lines[i].contains('|') {
            i += 1;
            continue;
        }
        let header = split_pipe_row(lines[i]);
        if header.is_empty() || !is_separator_row(lines[i + 1], header.len()) {
            i += 1;
            continue;
        }
        let mut rows = Vec::new();
        let mut j = i + 2;
        while j < lines.len() && is_pipe_row(lines[j]) {
            rows.push(split_pipe_row(lines[j]));
            j += 1;
        }
        tables.push(ParsedTable { header, rows });
        i = j;
    }
    tables
}

fn is_pipe_row(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.contains('|')
}

fn is_separator_row(line: &str, expected_cells: usize) -> bool {
    if !is_pipe_row(line) {
        return false;
    }
    let cells = split_pipe_row(line);
    cells.len() == expected_cells
        && cells.iter().all(|c| {
            let c = c.trim();
            !c.is_empty() && c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn split_pipe_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed.split('|').map(|c| c.trim().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_column_key_collapses_punctuation_and_case() {
        assert_eq!(normalize_column_key("First Name"), "first_name");
        assert_eq!(normalize_column_key("  Company  "), "company");
        assert_eq!(normalize_column_key("First-Name"), "first_name");
        assert_eq!(normalize_column_key("Löwe"), "l_we");
    }

    #[test]
    fn normalize_header_disambiguates_collisions() {
        assert_eq!(normalize_header(&["Name".to_string(), "name".to_string()]), vec!["name", "name_2"]);
    }

    #[test]
    fn find_html_tables_reads_a_th_header_and_strips_nested_tags() {
        let html = "<table><tr><th>Name</th><th>Score</th></tr><tr><td><b>Alice</b></td><td>10</td></tr></table>";
        let tables = find_html_tables(html);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].header, vec!["Name", "Score"]);
        assert_eq!(tables[0].rows, vec![vec!["Alice".to_string(), "10".to_string()]]);
    }

    #[test]
    fn find_html_tables_falls_back_to_the_first_tr_when_no_th_exists() {
        let html = "<table><tr><td>Name</td><td>Score</td></tr><tr><td>Bob</td><td>20</td></tr></table>";
        let tables = find_html_tables(html);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].header, vec!["Name", "Score"]);
        assert_eq!(tables[0].rows, vec![vec!["Bob".to_string(), "20".to_string()]]);
    }

    #[test]
    fn find_html_tables_does_not_confuse_thead_with_th() {
        let html = "<table><thead><tr><th>Name</th></tr></thead><tbody><tr><td>Carol</td></tr></tbody></table>";
        let tables = find_html_tables(html);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].header, vec!["Name"]);
        assert_eq!(tables[0].rows, vec![vec!["Carol".to_string()]]);
    }

    #[test]
    fn find_html_tables_decodes_entities() {
        let html = "<table><tr><th>Company</th></tr><tr><td>Peter&#39;s Pool &amp; Spa</td></tr></table>";
        let tables = find_html_tables(html);
        assert_eq!(tables[0].rows, vec![vec!["Peter's Pool & Spa".to_string()]]);
    }

    #[test]
    fn find_markdown_tables_parses_a_pipe_table() {
        let md = "| Name | Score |\n|---|---|\n| Dana | 40 |\n| Eve | 50 |";
        let tables = find_markdown_tables(md);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].header, vec!["Name", "Score"]);
        assert_eq!(tables[0].rows, vec![vec!["Dana".to_string(), "40".to_string()], vec!["Eve".to_string(), "50".to_string()]]);
    }

    #[test]
    fn find_tables_in_text_reports_two_when_two_tables_are_present() {
        let text = "<table><tr><th>A</th></tr><tr><td>1</td></tr></table> and \
                    <table><tr><th>B</th></tr><tr><td>2</td></tr></table>";
        assert_eq!(find_tables_in_text(text).len(), 2);
    }

    #[test]
    fn find_tables_in_text_reports_zero_for_plain_prose() {
        assert_eq!(find_tables_in_text("just some prose, no tables here").len(), 0);
    }

    #[test]
    fn discover_tabular_field_finds_a_table_nested_inside_an_object_field() {
        let payload = serde_json::json!({
            "metadata": {"source": "notion"},
            "text": "<table><tr><th>First</th><th>Company</th></tr><tr><td>Peter</td><td>Pete's Pools</td></tr></table>",
        });
        let found = discover_tabular_field(&payload, 6, &["first".to_string()])
            .expect("exactly one table exists, nested under `text`");
        assert_eq!(found.field_path, "text");
        assert_eq!(found.columns, vec!["first", "company"]);
        assert_eq!(found.rows, vec![serde_json::json!({"first": "Peter", "company": "Pete's Pools"})]);
    }

    #[test]
    fn discover_tabular_field_refuses_to_guess_between_two_tables() {
        let payload = serde_json::json!({
            "a": "<table><tr><th>X</th></tr><tr><td>1</td></tr></table>",
            "b": "<table><tr><th>Y</th></tr><tr><td>2</td></tr></table>",
        });
        assert!(discover_tabular_field(&payload, 6, &["x".to_string()]).is_none());
    }

    #[test]
    fn discover_tabular_field_returns_none_for_no_tables() {
        let payload = serde_json::json!({"text": "no markup here at all"});
        assert!(discover_tabular_field(&payload, 6, &["first".to_string()]).is_none());
    }

    #[test]
    fn discover_tabular_field_skips_blank_template_rows() {
        // 1 real row + 4 blank placeholder rows (the exact shape a Notion
        // table template leaves behind) — only the real row must survive.
        let html = "<table><tr><th>First</th><th>Company</th></tr>\
            <tr><td>Peter</td><td>Pete's Pools</td></tr>\
            <tr><td></td><td></td></tr>\
            <tr><td></td><td></td></tr>\
            <tr><td></td><td></td></tr>\
            <tr><td></td><td></td></tr></table>";
        let payload = serde_json::json!({ "text": html });
        let found = discover_tabular_field(&payload, 6, &["first".to_string()])
            .expect("one real row plus blank template rows is still exactly one recognizable table");
        assert_eq!(found.rows, vec![serde_json::json!({"first": "Peter", "company": "Pete's Pools"})]);
    }

    #[test]
    fn discover_tabular_field_skips_a_row_with_only_stray_non_identity_content() {
        // The identity column ("first") is empty, but "company" — not an
        // identity field — has leftover text. No identity means no stable
        // entity, so this row must still be dropped even though it isn't
        // fully blank.
        let html = "<table><tr><th>First</th><th>Company</th></tr>\
            <tr><td>Peter</td><td>Pete's Pools</td></tr>\
            <tr><td></td><td>stray note, no name attached</td></tr></table>";
        let payload = serde_json::json!({ "text": html });
        let found = discover_tabular_field(&payload, 6, &["first".to_string()])
            .expect("exactly one recognizable table");
        assert_eq!(found.rows, vec![serde_json::json!({"first": "Peter", "company": "Pete's Pools"})]);
    }

    #[test]
    fn discover_tabular_field_treats_nbsp_and_br_only_cells_as_blank() {
        let html = "<table><tr><th>First</th><th>Company</th></tr>\
            <tr><td>Peter</td><td>Pete's Pools</td></tr>\
            <tr><td>&nbsp;</td><td>&nbsp;</td></tr>\
            <tr><td><br></td><td><p></p></td></tr></table>";
        let payload = serde_json::json!({ "text": html });
        let found = discover_tabular_field(&payload, 6, &["first".to_string()])
            .expect("exactly one recognizable table");
        assert_eq!(found.rows, vec![serde_json::json!({"first": "Peter", "company": "Pete's Pools"})]);
    }

    #[test]
    fn discover_tabular_field_treats_zero_width_only_cells_as_blank() {
        let html = "<table><tr><th>First</th><th>Company</th></tr>\
            <tr><td>Peter</td><td>Pete's Pools</td></tr>\
            <tr><td>\u{200B}</td><td>\u{FEFF}</td></tr></table>";
        let payload = serde_json::json!({ "text": html });
        let found = discover_tabular_field(&payload, 6, &["first".to_string()])
            .expect("exactly one recognizable table");
        assert_eq!(found.rows, vec![serde_json::json!({"first": "Peter", "company": "Pete's Pools"})]);
    }

    #[test]
    fn filter_blank_identity_rows_is_a_no_op_when_identity_columns_is_empty() {
        let rows = vec![serde_json::json!({"first": "", "company": ""})];
        assert_eq!(filter_blank_identity_rows(rows.clone(), &[]), rows);
    }
}
