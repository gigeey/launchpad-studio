//! Grep tool — fast in-process content search built on the ripgrep library
//! crates: `grep_regex` for the matcher, `grep_searcher` for the search
//! engine, and `ignore` for `.gitignore`-honoring directory traversal and
//! file-type filters. These crates are linked directly rather than shelling
//! out to a vendored `rg` binary, so each search avoids the fork/exec cost of
//! spawning a subprocess while producing the same results ripgrep would.
//!
//! Behavior summary:
//! - Honors `.gitignore` by default (via `ignore::WalkBuilder`).
//! - Three output modes: `files_with_matches` (default), `content`, `count`.
//! - File filtering by `glob` (override pattern) and/or `type` (ripgrep-style
//!   file types: `rust`, `js`, …).
//! - Context flags `-A`, `-B`, `-C`, `context` only valid with content
//!   mode; passing them with another mode is a recoverable error.
//! - `-n` line numbers are on by default in content mode; silently ignored
//!   otherwise.
//! - `-i` case-insensitive and `multiline` flags are forwarded to both
//!   the matcher and the searcher.
//! - Integer (`-A`/`-B`/`-C`/`context`/`head_limit`/`offset`) and boolean
//!   (`-i`/`-n`/`multiline`) arguments are accepted as either JSON values or
//!   JSON strings, because some providers serialize them as strings.
//! - `offset` is applied first, then `head_limit`. `head_limit = 0` is the
//!   explicit unlimited escape hatch.
//!
//! Cancellation: `RunnerContext::cancel` is checked at the top of the
//! search and from inside the per-file walk loop. The branch's `AoError`
//! enum lacks a `Cancelled` variant, so cancellation surfaces as
//! `AoError::Internal("cancelled")` — see [`CANCELLED_MESSAGE`]. Same
//! marker as `read::CANCELLED_MESSAGE` and `glob::CANCELLED_MESSAGE`, so
//! a single grep across the crate flips all three when `Cancelled` lands
//! on `main`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub mod prompt;

/// Default number of result entries returned when `head_limit` is omitted.
/// Keeps a single search result bounded to a manageable page size unless the
/// caller explicitly asks for more (or `0` for unlimited).
pub const DEFAULT_HEAD_LIMIT: usize = 250;

/// Walker entries between cancellation polls. Cheap; chosen so that even
/// a 1M-file flat directory cancels well within the 100 ms PRD budget.
const CANCEL_POLL_FILES: usize = 32;

/// Text returned when no file matches are found in `files_with_matches` mode.
pub const NO_FILES_MESSAGE: &str = "No files found";

/// Text returned when no matches are found in `content` or `count` mode.
pub const NO_MATCHES_MESSAGE: &str = "No matches found";

/// String surfaced through `AoError::Internal` when a Grep invocation is
/// cancelled mid-search. Promoted to `AoError::Cancelled` once that
/// variant lands on `main`. Same marker as `read::CANCELLED_MESSAGE` and
/// `glob::CANCELLED_MESSAGE`.
pub const CANCELLED_MESSAGE: &str = "cancelled";

/// `--` separator emitted between non-contiguous context blocks within a
/// single file in content mode. Mirrors ripgrep's CLI output.
const CONTEXT_BREAK_MARKER: &str = "--";

/// Maximum characters per matched line in content mode. Lines longer than
/// this threshold are replaced with a `[long line N chars]` marker so that
/// a single hit on a minified bundle line cannot exhaust the result budget.
/// Counted in Unicode scalar values (chars), not bytes.
pub const MAX_LINE_CHARS: usize = 500;

/// Maximum rendered body size in bytes. Results exceeding this limit are
/// truncated at a line boundary and a `[truncated]` marker is appended so
/// the caller can tell the result is incomplete.
pub const MAX_RESULT_BYTES: usize = 102_400;

/// Default per-search timeout in seconds. Override via the
/// `LAUNCHPAD_GREP_TIMEOUT_SECS` environment variable (u64; invalid or zero
/// values fall back to this default without error).
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

// Test-only thread-locals. Thread-local storage ensures each test's overrides
// are invisible to other tests running in parallel.
//
//   TEST_DELAY_MS  — async sleep inserted in `invoke` before `spawn_blocking`,
//                    simulating a slow pre-search so the timeout fires in tests.
//   TEST_TIMEOUT_MS — when non-zero, overrides the per-search timeout with a
//                    millisecond value (faster than the 1 s env-var minimum).
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_DELAY_MS: std::cell::Cell<u64> = std::cell::Cell::new(0);
    pub(crate) static TEST_TIMEOUT_MS: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    FilesWithMatches,
    Content,
    Count,
}

#[derive(Debug)]
struct GrepOptions {
    pattern: String,
    path: PathBuf,
    /// Canonicalized runner cwd used to relativize output paths. Stored
    /// once at parse time so the blocking search task needs no env I/O.
    cwd: PathBuf,
    glob: Option<String>,
    file_type: Option<String>,
    mode: OutputMode,
    case_insensitive: bool,
    show_line_numbers: bool,
    multiline: bool,
    before_context: usize,
    after_context: usize,
    /// `0` is the explicit "unlimited" escape hatch.
    head_limit: usize,
    offset: usize,
}

/// Searches file contents using the ripgrep library crates in-process.
/// Read-only; `is_concurrency_safe()` is true.
pub struct Grep;

#[async_trait]
impl IoTool for Grep {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regular expression pattern to search for in file contents."
                },
                "path": {
                    "type": "string",
                    "description": "Path to a file or directory to search in. May be absolute, `~`-prefixed, or relative to the runner's current directory. Defaults to the current working directory when omitted."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")."
                },
                "type": {
                    "type": "string",
                    "description": "File type filter (e.g. \"rust\", \"js\", \"py\")."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "description": "Output mode. Defaults to \"files_with_matches\"."
                },
                "-A": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines of context to show after each match (content mode only)."
                },
                "-B": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines of context to show before each match (content mode only)."
                },
                "-C": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines of context to show before AND after each match (content mode only)."
                },
                "context": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Alias for -C."
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case-insensitive search."
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers in content mode (default true)."
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Allow patterns to match across newlines."
                },
                "head_limit": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Maximum entries returned (0 = unlimited). Defaults to 250."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Skip the first N entries before applying head_limit."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let opts = match parse_options(&input) {
            Ok(o) => o,
            Err(msg) => return Ok(ToolOutput::error(msg, true)),
        };

        // Reject UNC roots (`\\server\share`) before touching the
        // filesystem — opening one triggers an NTLM auth handshake against
        // the named host, which would let a model-driven path coerce auth
        // material toward an attacker-chosen server.
        #[cfg(windows)]
        if is_unc_path(&opts.path) {
            return Ok(ToolOutput::error("UNC paths are not supported", true));
        }

        match tokio::fs::metadata(&opts.path).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut msg = format!("path does not exist: {}", opts.path.display());
                let suggestions = crate::glob::suggest_siblings(&opts.path);
                if let Some(suggestion) = suggestions.into_iter().next() {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    msg.push_str(&format!(
                        ". Did you mean {}? Search relative to {}.",
                        suggestion,
                        cwd.display()
                    ));
                }
                return Ok(ToolOutput::error(msg, true));
            }
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("failed to stat {}: {}", opts.path.display(), err),
                    true,
                ));
            }
        }

        if ctx.cancel.is_cancelled() {
            return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
        }

        // Parse per-search timeout from env; invalid / zero values fall back
        // to DEFAULT_TIMEOUT_SECS without error.
        let timeout_secs = std::env::var("LAUNCHPAD_GREP_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        // In tests, a thread-local millisecond override allows sub-second
        // timeout testing without setting a small env-var value that would
        // leak to and affect parallel tests.
        let timeout_dur = {
            #[cfg(test)]
            {
                let ms = TEST_TIMEOUT_MS.with(|c| c.get());
                if ms > 0 {
                    Duration::from_millis(ms)
                } else {
                    Duration::from_secs(timeout_secs)
                }
            }
            #[cfg(not(test))]
            Duration::from_secs(timeout_secs)
        };

        // The walker and searcher are blocking; run them on a blocking
        // thread so we don't stall the executor while crawling large
        // trees. Pass the cancel token clone so the worker can abort.
        //
        // The timeout wraps the entire async work unit so that a test-only
        // pre-search delay (TEST_DELAY_MS) is also subject to the deadline.
        let cancel = ctx.cancel.clone();
        let timed = tokio::time::timeout(timeout_dur, async move {
            // In tests, an async pre-search sleep simulates a slow invoke so
            // the timeout fires without creating large file fixtures.
            // Thread-local: invisible to other tests running in parallel.
            #[cfg(test)]
            {
                let delay_ms = TEST_DELAY_MS.with(|c| c.get());
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
            tokio::task::spawn_blocking(move || run_search(opts, cancel))
                .await
                .map_err(|e| AoError::Internal(format!("grep task panicked: {}", e)))?
        });
        match timed.await {
            Ok(result) => result,
            Err(_elapsed) => Ok(ToolOutput::error(
                format!("grep timed out after {}s", timeout_secs),
                true,
            )),
        }
    }
}

/// Read an optional unsigned-integer argument that may arrive either as a JSON
/// number or as a JSON string.
///
/// Some providers serialize integer tool arguments as strings (e.g.
/// `"head_limit": "50"` instead of `50`). A strict `Value::as_u64` returns
/// `None` for the string form, which would silently drop the argument and fall
/// back to a default the model never asked for. Accepting both encodings keeps
/// numeric flags honored regardless of how the provider framed them.
fn coerce_u64(input: &Value, key: &str) -> Option<u64> {
    let value = input.get(key)?;
    if let Some(n) = value.as_u64() {
        return Some(n);
    }
    value.as_str().and_then(|s| s.trim().parse::<u64>().ok())
}

/// Read an optional boolean argument that may arrive as a JSON bool, a
/// string-encoded bool (`"true"`/`"false"`, case-insensitive), or `1`/`0`.
///
/// Same motivation as [`coerce_u64`]: a provider emitting `"-i": "true"` must
/// still enable case-insensitive search rather than silently defaulting to the
/// case-sensitive path. Unrecognized strings yield `None` so the caller's
/// default applies.
fn coerce_bool(input: &Value, key: &str) -> Option<bool> {
    let value = input.get(key)?;
    if let Some(b) = value.as_bool() {
        return Some(b);
    }
    if let Some(s) = value.as_str() {
        return match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        };
    }
    value.as_u64().map(|n| n != 0)
}

/// Format a count with a grammatically correct noun: `1 file`, `2 files`.
fn count_noun(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

fn parse_options(input: &Value) -> Result<GrepOptions, String> {
    let pattern = match input.get("pattern").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        Some(_) => return Err("pattern must be a non-empty string".to_string()),
        None => return Err("pattern is required and must be a string".to_string()),
    };

    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to read current directory: {}", err))?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let path = match input.get("path").and_then(Value::as_str) {
        Some(p) => crate::path::expand_path(p, &cwd),
        None => cwd.clone(),
    };

    let glob = input.get("glob").and_then(Value::as_str).map(String::from);
    let file_type = input.get("type").and_then(Value::as_str).map(String::from);

    let mode = match input.get("output_mode").and_then(Value::as_str) {
        None => OutputMode::FilesWithMatches,
        Some("files_with_matches") => OutputMode::FilesWithMatches,
        Some("content") => OutputMode::Content,
        Some("count") => OutputMode::Count,
        Some(other) => {
            return Err(format!(
                "output_mode must be one of files_with_matches | content | count, got {}",
                other
            ));
        }
    };

    let case_insensitive = coerce_bool(input, "-i").unwrap_or(false);
    let show_line_numbers = coerce_bool(input, "-n").unwrap_or(true);
    let multiline = coerce_bool(input, "multiline").unwrap_or(false);

    let raw_a = coerce_u64(input, "-A");
    let raw_b = coerce_u64(input, "-B");
    let raw_c = coerce_u64(input, "-C");
    let raw_context = coerce_u64(input, "context");

    let context_used =
        raw_a.is_some() || raw_b.is_some() || raw_c.is_some() || raw_context.is_some();
    if context_used && mode != OutputMode::Content {
        return Err("-A, -B, -C, and context require output_mode = \"content\"".to_string());
    }

    // -C / context override -A and -B: a symmetric context window takes
    // precedence over separately-specified before/after counts.
    let (before_context, after_context) = if let Some(c) = raw_context.or(raw_c) {
        (c as usize, c as usize)
    } else {
        (
            raw_b.map(|n| n as usize).unwrap_or(0),
            raw_a.map(|n| n as usize).unwrap_or(0),
        )
    };

    let head_limit = match coerce_u64(input, "head_limit") {
        Some(n) => n as usize,
        None => DEFAULT_HEAD_LIMIT,
    };
    let offset = coerce_u64(input, "offset")
        .map(|n| n as usize)
        .unwrap_or(0);

    Ok(GrepOptions {
        pattern,
        path,
        cwd,
        glob,
        file_type,
        mode,
        case_insensitive,
        show_line_numbers,
        multiline,
        before_context,
        after_context,
        head_limit,
        offset,
    })
}

/// Split a user-supplied glob string into individual pattern tokens.
///
/// Whitespace and top-level commas are treated as separators. Commas inside
/// brace expressions (e.g. `*.{ts,tsx}`) are NOT split because the brace
/// depth counter is non-zero at those positions.
///
/// Examples:
///   `"*.ts *.tsx"`     → `["*.ts", "*.tsx"]`
///   `"*.ts,*.tsx"`     → `["*.ts", "*.tsx"]`
///   `"*.{ts,tsx}"`     → `["*.{ts,tsx}"]`   (brace preserved)
///   `"*.ts, *.tsx src/**/*.js"` → `["*.ts", "*.tsx", "src/**/*.js"]`
fn split_glob_patterns(input: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut brace_depth: i32 = 0;
    for ch in input.chars() {
        match ch {
            '{' => {
                brace_depth += 1;
                current.push(ch);
            }
            '}' => {
                brace_depth -= 1;
                current.push(ch);
            }
            ',' | ' ' | '\t' | '\n' if brace_depth == 0 => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn run_search(opts: GrepOptions, cancel: CancellationToken) -> Result<ToolOutput, AoError> {
    if cancel.is_cancelled() {
        return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
    }

    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .multi_line(opts.multiline)
        .dot_matches_new_line(opts.multiline)
        .build(&opts.pattern)
    {
        Ok(m) => m,
        Err(err) => {
            return Ok(ToolOutput::error(
                format!("invalid regex pattern '{}': {}", opts.pattern, err),
                true,
            ));
        }
    };

    let mut wb = WalkBuilder::new(&opts.path);
    // `require_git(false)` makes `.gitignore` semantics apply outside git
    // repos too — important for tempdir-based callers and tests.
    wb.require_git(false);
    // Walk hidden files (dotfiles like .env.example, .github/workflows) so
    // real config files are searchable. Paired with the VCS deny-list built
    // below — the two changes must land together because hidden=false without
    // the deny-list floods results with .git/objects binary blobs.
    wb.hidden(false);

    if let Some(t) = &opts.file_type {
        let mut tb = TypesBuilder::new();
        tb.add_defaults();
        tb.select(t);
        match tb.build() {
            Ok(types) => {
                wb.types(types);
            }
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("invalid type filter '{}': {}", t, err),
                    true,
                ));
            }
        }
    }

    // Always build an OverrideBuilder. The user glob (if any) is added first;
    // VCS deny-list patterns are added last so they win over any user glob
    // that might otherwise match VCS directory names (e.g. glob ".*").
    let mut ob = OverrideBuilder::new(&opts.path);
    if let Some(g) = &opts.glob {
        for token in split_glob_patterns(g) {
            if let Err(err) = ob.add(&token) {
                return Ok(ToolOutput::error(
                    format!("invalid glob filter '{}': {}", token, err),
                    true,
                ));
            }
        }
    }
    for pattern in &["!.git", "!.svn", "!.hg", "!.bzr", "!.jj", "!.sl"] {
        ob.add(pattern)
            .expect("static VCS deny pattern is always valid");
    }
    match ob.build() {
        Ok(ovr) => {
            wb.overrides(ovr);
        }
        Err(err) => {
            return Ok(ToolOutput::error(
                format!("override builder error: {}", err),
                true,
            ));
        }
    }

    let mut searcher = SearcherBuilder::new()
        .multi_line(opts.multiline)
        .line_number(true)
        .before_context(opts.before_context)
        .after_context(opts.after_context)
        .build();

    let walker = wb.build();

    match opts.mode {
        OutputMode::FilesWithMatches => {
            run_files_with_matches(walker, &mut searcher, &matcher, &opts, &cancel)
        }
        OutputMode::Count => run_count(walker, &mut searcher, &matcher, &opts, &cancel),
        OutputMode::Content => run_content(walker, &mut searcher, &matcher, &opts, &cancel),
    }
}

/// Walk every file under `walker`, polling `cancel` between batches.
/// Errors from individual `DirEntry` reads (permission denied on a
/// subdir, …) are skipped so a single bad entry can't fail the whole
/// search; cancellation propagates as `AoError::Internal("cancelled")`.
fn for_each_file(
    walker: ignore::Walk,
    cancel: &CancellationToken,
    mut visit: impl FnMut(&ignore::DirEntry) -> Result<(), AoError>,
) -> Result<(), AoError> {
    for (i, entry_res) in walker.enumerate() {
        if i % CANCEL_POLL_FILES == 0 && cancel.is_cancelled() {
            return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
        }
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        visit(&entry)?;
    }
    Ok(())
}

fn run_files_with_matches(
    walker: ignore::Walk,
    searcher: &mut Searcher,
    matcher: &grep_regex::RegexMatcher,
    opts: &GrepOptions,
    cancel: &CancellationToken,
) -> Result<ToolOutput, AoError> {
    let mut hits: Vec<(PathBuf, SystemTime)> = Vec::new();

    for_each_file(walker, cancel, |entry| {
        let path = entry.path().to_path_buf();
        let mut sink = FileMatchSink { matched: false };
        // I/O errors on a single file (deleted between walk and search,
        // permission denied, …) shouldn't kill the whole search.
        if searcher.search_path(matcher, &path, &mut sink).is_err() {
            return Ok(());
        }
        if sink.matched {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            hits.push((path, mtime));
        }
        Ok(())
    })?;

    // Newest first; lexicographic on path as a tiebreak when mtimes
    // coincide (filesystem timestamp resolution).
    hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let (sliced, head_limit_truncated) = apply_offset_limit(hits, opts.offset, opts.head_limit);

    if sliced.is_empty() {
        return Ok(ToolOutput::text(NO_FILES_MESSAGE));
    }

    let file_count = sliced.len();
    let mut out = format!("Found {}\n", count_noun(file_count, "file"));
    for (i, (p, _)) in sliced.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&crate::path::relativize_path(p, &opts.cwd));
    }
    if head_limit_truncated || opts.offset > 0 {
        out.push('\n');
        out.push_str(&pagination_footer(opts.head_limit, opts.offset));
    }
    Ok(ToolOutput::text(truncate_to_byte_cap(out)))
}

fn run_count(
    walker: ignore::Walk,
    searcher: &mut Searcher,
    matcher: &grep_regex::RegexMatcher,
    opts: &GrepOptions,
    cancel: &CancellationToken,
) -> Result<ToolOutput, AoError> {
    let mut counts: Vec<(PathBuf, u64)> = Vec::new();

    for_each_file(walker, cancel, |entry| {
        let path = entry.path().to_path_buf();
        let mut sink = CountSink { count: 0 };
        if searcher.search_path(matcher, &path, &mut sink).is_err() {
            return Ok(());
        }
        if sink.count > 0 {
            counts.push((path, sink.count));
        }
        Ok(())
    })?;

    // Most matches first; lexicographic on path as a tiebreak so the
    // output is deterministic across runs.
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let (sliced, head_limit_truncated) = apply_offset_limit(counts, opts.offset, opts.head_limit);

    if sliced.is_empty() {
        return Ok(ToolOutput::text(NO_MATCHES_MESSAGE));
    }

    let total_count: u64 = sliced.iter().map(|(_, n)| n).sum();
    let file_count = sliced.len();

    let mut out = String::new();
    for (i, (p, n)) in sliced.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&crate::path::relativize_path(p, &opts.cwd));
        out.push(':');
        out.push_str(&n.to_string());
    }
    out.push('\n');
    out.push_str(&format!(
        "Found {total_count} total occurrences across {}",
        count_noun(file_count, "file")
    ));
    if head_limit_truncated || opts.offset > 0 {
        out.push('\n');
        out.push_str(&pagination_footer(opts.head_limit, opts.offset));
    }
    Ok(ToolOutput::text(truncate_to_byte_cap(out)))
}

fn run_content(
    walker: ignore::Walk,
    searcher: &mut Searcher,
    matcher: &grep_regex::RegexMatcher,
    opts: &GrepOptions,
    cancel: &CancellationToken,
) -> Result<ToolOutput, AoError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut all_lines: Vec<ContentLine> = Vec::new();

    for_each_file(walker, cancel, |entry| {
        let path = entry.path().to_path_buf();
        let mut sink = ContentSink {
            cancel: cancel.clone(),
            cancelled: cancelled.clone(),
            path: path.clone(),
            lines: Vec::new(),
            pending_break: false,
        };
        let _ = searcher.search_path(matcher, &path, &mut sink);
        if cancelled.load(Ordering::SeqCst) {
            return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
        }
        all_lines.extend(sink.lines);
        Ok(())
    })?;

    // Apply offset + head_limit globally across the flattened result
    // stream: collect every matching item first, then slice by offset and
    // limit so pagination is consistent across the whole result set.
    let (sliced, head_limit_truncated) =
        apply_offset_limit(all_lines, opts.offset, opts.head_limit);

    if sliced.is_empty() {
        return Ok(ToolOutput::text(NO_MATCHES_MESSAGE));
    }

    let mut body = format_content_lines(&sliced, opts.show_line_numbers, &opts.cwd);
    if head_limit_truncated || opts.offset > 0 {
        body.push('\n');
        body.push_str(&pagination_footer(opts.head_limit, opts.offset));
    }
    Ok(ToolOutput::text(truncate_to_byte_cap(body)))
}

/// Apply `offset` then `head_limit` to `items`. Returns the sliced vec and a
/// bool indicating whether `head_limit` truncated the result (i.e., there were
/// more items after offset than the limit allowed). When `limit == 0` (the
/// explicit "unlimited" escape hatch) truncation is never reported.
fn apply_offset_limit<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, bool) {
    if offset >= items.len() {
        return (Vec::new(), false);
    }
    let mut out: Vec<T> = items.into_iter().skip(offset).collect();
    let head_limit_truncated = limit > 0 && out.len() > limit;
    if head_limit_truncated {
        out.truncate(limit);
    }
    (out, head_limit_truncated)
}

/// One-line pagination footer appended when `head_limit` truncated the result
/// or when a non-zero `offset` was supplied. Machine-parseable so the model
/// can extract the values and issue a follow-up call.
fn pagination_footer(limit: usize, offset: usize) -> String {
    format!("[paginated: limit={limit} offset={offset}]")
}

#[derive(Debug, Clone)]
struct ContentLine {
    path: PathBuf,
    line_number: u64,
    text: String,
    /// True if a `--` separator should be emitted before this line.
    /// Set on the line that follows a `Sink::context_break` callback.
    break_before: bool,
}

fn format_content_lines(
    lines: &[ContentLine],
    show_line_numbers: bool,
    cwd: &std::path::Path,
) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if line.break_before && i > 0 {
            out.push('\n');
            out.push_str(CONTEXT_BREAK_MARKER);
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&crate::path::relativize_path(&line.path, cwd));
        out.push(':');
        if show_line_numbers {
            out.push_str(&line.line_number.to_string());
            out.push(':');
        }
        out.push_str(&line.text);
    }
    out
}

// --- Sinks -----------------------------------------------------------------

struct FileMatchSink {
    matched: bool,
}

impl Sink for FileMatchSink {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> std::io::Result<bool> {
        self.matched = true;
        // First match is enough — short-circuit to avoid scanning the rest.
        Ok(false)
    }
}

struct CountSink {
    count: u64,
}

impl Sink for CountSink {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> std::io::Result<bool> {
        self.count += 1;
        Ok(true)
    }
}

struct ContentSink {
    cancel: CancellationToken,
    cancelled: Arc<AtomicBool>,
    path: PathBuf,
    lines: Vec<ContentLine>,
    pending_break: bool,
}

impl ContentSink {
    fn check_cancel(&mut self) -> bool {
        if self.cancel.is_cancelled() {
            self.cancelled.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }

    fn push(&mut self, line_number: u64, bytes: &[u8]) {
        let break_before = std::mem::take(&mut self.pending_break);
        let raw = String::from_utf8_lossy(strip_trailing_newline(bytes)).to_string();
        let char_count = raw.chars().count();
        let text = if char_count > MAX_LINE_CHARS {
            format!("[long line {} chars]", char_count)
        } else {
            raw
        };
        self.lines.push(ContentLine {
            path: self.path.clone(),
            line_number,
            text,
            break_before,
        });
    }
}

impl Sink for ContentSink {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> std::io::Result<bool> {
        if self.check_cancel() {
            return Ok(false);
        }
        self.push(m.line_number().unwrap_or(0), m.bytes());
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> std::io::Result<bool> {
        if self.check_cancel() {
            return Ok(false);
        }
        // Before/After/Other render the same way; ripgrep's CLI uses `-`
        // vs `:` separators, but our shape is `path:lineno:line` for
        // every line, per this tool's contract.
        self.push(c.line_number().unwrap_or(0), c.bytes());
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> std::io::Result<bool> {
        self.pending_break = true;
        Ok(true)
    }
}

fn strip_trailing_newline(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

/// Clamp `body` to [`MAX_RESULT_BYTES`]. If the body exceeds the cap, trim
/// at the last newline at or before the boundary (never splitting mid-line)
/// and append a `[truncated]` marker on its own line.
fn truncate_to_byte_cap(body: String) -> String {
    if body.len() <= MAX_RESULT_BYTES {
        return body;
    }
    // Walk back to a valid UTF-8 char boundary (at most 3 steps for any
    // valid UTF-8 encoding; body is always valid UTF-8 as a String).
    let mut boundary = MAX_RESULT_BYTES;
    while !body.is_char_boundary(boundary) {
        boundary -= 1;
    }
    // Prefer a newline boundary so we never cut mid-line.
    let cut = body[..boundary]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(boundary);
    let mut out = body[..cut].to_string();
    out.push_str("[truncated]");
    out
}

/// True when `path` looks like a Windows UNC root (`\\server\share`). The
/// check is on the leading two backslashes only — verbatim variants
/// (`\\?\...`, `\\.\...`) share the same prefix and are also refused,
/// which is the cautious choice for a tool walking model-supplied paths.
#[cfg(windows)]
fn is_unc_path(path: &std::path::Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with(r"\\")
}

#[cfg(test)]
mod tests;
