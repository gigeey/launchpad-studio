//! Glob tool — fast file-pattern matching over a directory tree.
//!
//! Compiles `pattern` with `globset`, walks under `path` (or current dir)
//! with the `ignore` crate so `.gitignore` semantics are honored by default,
//! collects every file whose path-relative-to-root matches the pattern,
//! sorts by mtime descending, and caps the result at [`MAX_RESULTS`]. When
//! the cap is hit a [`TRUNCATION_NOTICE`] trailer is appended to the Text
//! output so the caller knows results were elided.
//!
//! Cancellation: `RunnerContext::cancel` is polled every
//! [`CANCEL_POLL_BATCH`] walker entries. The branch's `AoError` enum lacks
//! a `Cancelled` variant, so cancellation surfaces as
//! `AoError::Internal("cancelled")` — see [`CANCELLED_MESSAGE`]. When
//! `Cancelled` lands on `main` this will switch over (the same marker
//! string is used by the Read tool, so a single grep handles both).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::{json, Value};

pub mod prompt;

/// Maximum number of paths returned in a single call. We use 1000 — larger
/// than a typical interactive listing — because the tool runs in-process and
/// pays no per-result transport cost, so a generous cap rarely truncates while
/// still bounding pathological patterns.
pub const MAX_RESULTS: usize = 1000;

/// Maximum number of output bytes (~100 KB) emitted by a single call.
/// Second guard layered on top of [`MAX_RESULTS`] so a pathological
/// pattern matching long-named paths cannot overrun the model's context
/// even when result count stays under the count cap. Reaching this cap
/// appends a [`byte_cap_trailer`] line so the caller can tell which
/// guard fired.
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// Walker entries between cancellation polls. Cheap; chosen so that even a
/// flat 1M-file directory cancels well within the 100 ms PRD budget.
const CANCEL_POLL_BATCH: usize = 256;

/// Trailer Text appended to the joined paths when [`MAX_RESULTS`] is hit, so
/// the caller knows the listing was elided and can narrow the pattern/path.
pub const TRUNCATION_NOTICE: &str =
    "(Results are truncated. Consider using a more specific path or pattern.)";

/// Text returned when no files match, so callers see a stable, explicit
/// message instead of an empty string they might mistake for a transport error.
pub const NO_RESULTS_MESSAGE: &str = "No files found";

/// String surfaced through `AoError::Internal` when a Glob invocation is
/// cancelled mid-walk. Promoted to `AoError::Cancelled` once that variant
/// lands on `main`. Same marker as `read::CANCELLED_MESSAGE`.
pub const CANCELLED_MESSAGE: &str = "cancelled";

/// Env var that, when set to `false` (case-insensitive), disables all four
/// `ignore`-crate filtering mechanisms (`.gitignore`, `.ignore`, global git
/// excludes, and per-repo `.git/info/exclude`). Any other value — including
/// unset, empty, `true`, `1` — keeps the default ON.
pub const RESPECT_GITIGNORE_ENV: &str = "LAUNCHPAD_GLOB_RESPECT_GITIGNORE";

/// Matches files by glob pattern under a directory, sorted by mtime
/// descending. Read-only; `is_concurrency_safe()` is true.
pub struct Glob;

#[async_trait]
impl IoTool for Glob {
    fn name(&self) -> &str {
        "Glob"
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
                    "description": "The glob pattern to match files against (e.g. \"**/*.rs\")."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in. May be absolute, `~`-prefixed, or relative to the runner's current directory. Defaults to the current working directory when omitted."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Skip the first N entries of the sorted result list (default 0). For resuming a prior truncated result only. Ordering is stable within a single search root and a single call window."
                },
                "no_ignore": {
                    "type": "boolean",
                    "description": "When true, ignore-file rules (.gitignore, .ignore, global git excludes) are bypassed so that ignored files (e.g. build artifacts, files under target/ or node_modules/) are included in results. Defaults to false — ignored files are hidden. Set this to true when you are specifically looking for a file that may be gitignored and an ordinary search returns nothing."
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
        let pattern = match input.get("pattern").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            Some(_) => {
                return Ok(ToolOutput::error(
                    "pattern must be a non-empty string",
                    true,
                ));
            }
            None => {
                return Ok(ToolOutput::error(
                    "pattern is required and must be a string",
                    true,
                ));
            }
        };

        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;

        // Accept a native JSON bool or a string-encoded bool ("true"/"false",
        // case-insensitive) — some providers serialize booleans as strings.
        // Anything unrecognized (or absent) defaults to false (respect ignores).
        let no_ignore = match input.get("no_ignore") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
            _ => false,
        };

        let cwd = match std::env::current_dir() {
            Ok(d) => d,
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("failed to read current directory: {}", err),
                    true,
                ));
            }
        };
        // When `pattern` is absolute, peel the longest glob-free prefix
        // into the search root and use the tail as the matcher pattern.
        // When the user also supplies an explicit `path`, that wins as the
        // walk root — but the absolute prefix is still stripped off the
        // pattern (it is ignored, not errored on), so the tail becomes the
        // matcher and walks under the user's path.
        let path_arg = input.get("path").and_then(Value::as_str);
        let split = absolute_pattern_split(&pattern);
        let (search_root, effective_pattern): (PathBuf, String) = match (path_arg, split) {
            (Some(p), Some((_, tail))) => (crate::path::expand_path(p, &cwd), tail),
            (Some(p), None) => (crate::path::expand_path(p, &cwd), pattern.clone()),
            (None, Some((root, tail))) => (root, tail),
            (None, None) => (cwd.clone(), pattern.clone()),
        };

        // Reject UNC roots (`\\server\share`) before touching the
        // filesystem — opening one triggers an NTLM auth handshake against
        // the named host, which would let a model-driven path coerce auth
        // material toward an attacker-chosen server.
        #[cfg(windows)]
        if is_unc_path(&search_root) {
            return Ok(ToolOutput::error("UNC paths are not supported", true));
        }

        // Stat first so we can return a clean recoverable error before we
        // build the walker (which would just emit IO errors lazily).
        let metadata = match tokio::fs::metadata(&search_root).await {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(
                    missing_root_message(&search_root, &cwd),
                    true,
                ));
            }
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("failed to stat {}: {}", search_root.display(), err),
                    true,
                ));
            }
        };
        if !metadata.is_dir() {
            return Ok(ToolOutput::error(
                format!("path is not a directory: {}", search_root.display()),
                true,
            ));
        }

        let glob_pattern = match globset::Glob::new(&effective_pattern) {
            Ok(g) => g,
            Err(err) => {
                return Ok(ToolOutput::error(
                    format!("invalid glob pattern '{}': {}", effective_pattern, err),
                    true,
                ));
            }
        };
        let matcher = glob_pattern.compile_matcher();

        if ctx.cancel.is_cancelled() {
            return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
        }

        // The `ignore` crate's `WalkBuilder` honors `.gitignore`,
        // `.ignore`, and global git excludes by default; `require_git(false)`
        // makes that apply outside git repos too (so tempdir tests work).
        // `hidden(false)` walks dotfiles/dotdirs so patterns like `**/.*`
        // match `.env`, `.config/...`, etc.
        //
        // Ignore filtering can be bypassed two ways: the model can pass
        // `no_ignore: true` on a single call (per-call opt-out), or an operator
        // can set `RESPECT_GITIGNORE_ENV=false` (global kill switch). Default is
        // ON; either signal toward bypass wins, so a model that explicitly asks
        // to see ignored files is always honored.
        let respect = respect_gitignore() && !no_ignore;
        let walker = WalkBuilder::new(&search_root)
            .require_git(false)
            .hidden(false)
            .git_ignore(respect)
            .ignore(respect)
            .git_global(respect)
            .git_exclude(respect)
            .build();

        let mut hits: Vec<(PathBuf, SystemTime)> = Vec::new();

        for (i, entry_res) in walker.enumerate() {
            if i % CANCEL_POLL_BATCH == 0 && ctx.cancel.is_cancelled() {
                return Err(AoError::Internal(CANCELLED_MESSAGE.to_string()));
            }

            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            if path == search_root.as_path() {
                continue;
            }

            let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
            if !is_file {
                continue;
            }

            let rel = path.strip_prefix(&search_root).unwrap_or(path);
            if !matcher.is_match(rel) {
                continue;
            }

            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            hits.push((path.to_path_buf(), mtime));
        }

        // Newest first. Stable sort keeps lexicographic order as a tiebreak
        // when mtimes coincide (filesystem timestamp resolution).
        hits.sort_by(|a, b| b.1.cmp(&a.1));

        // Apply opt-in offset: skip the first N entries of the sorted list
        // before applying MAX_RESULTS and the byte budget. An oversized offset
        // (>= total matches) drains the vec and falls through to the empty
        // early-return below, which returns truncated: false — not an error.
        if offset > 0 {
            let skip = offset.min(hits.len());
            hits.drain(..skip);
        }

        if hits.is_empty() {
            return Ok(ToolOutput::structured(json!({
                "matches": [],
                "truncated": false,
                "search_root": search_root.to_string_lossy().into_owned(),
                "pattern": pattern,
                "text_fallback": NO_RESULTS_MESSAGE,
            })));
        }

        let mut max_results_truncated = false;
        if hits.len() > MAX_RESULTS {
            hits.truncate(MAX_RESULTS);
            max_results_truncated = true;
        }

        // Pre-format paths so the byte-cap accounting matches the bytes
        // we actually emit (relative paths under cwd are shorter than
        // their absolute form).
        let formatted: Vec<String> = hits.iter().map(|(p, _)| format_hit_path(p, &cwd)).collect();

        // Reserve room for the worst-case byte-cap trailer so output
        // stays under MAX_OUTPUT_BYTES even when the cap fires. Computed
        // against the upper bound on dropped count (every path could be
        // dropped) so the reservation is sound regardless of which path
        // boundary we stop at.
        let trailer_reserve = byte_cap_trailer(formatted.len()).len() + 1;

        let mut out = String::new();
        let mut emitted = 0usize;
        for (i, line) in formatted.iter().enumerate() {
            let separator = if i == 0 { 0 } else { 1 };
            let projected = out.len() + separator + line.len() + trailer_reserve;
            // Always emit at least one path: if a single line exceeds the
            // budget we'd otherwise return an empty body plus a trailer,
            // which is less useful than one oversized hit.
            if projected > MAX_OUTPUT_BYTES && emitted > 0 {
                break;
            }
            if i > 0 {
                out.push('\n');
            }
            out.push_str(line);
            emitted += 1;
        }

        let byte_cap_dropped = formatted.len() - emitted;
        if byte_cap_dropped > 0 {
            out.push('\n');
            out.push_str(&byte_cap_trailer(byte_cap_dropped));
        } else if max_results_truncated {
            out.push('\n');
            out.push_str(TRUNCATION_NOTICE);
        }

        let truncated = byte_cap_dropped > 0 || max_results_truncated;

        let structured_matches: Vec<serde_json::Value> = hits[..emitted]
            .iter()
            .map(|(p, t)| {
                let mtime_unix = t
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                json!({
                    "path": format_hit_path(p, &cwd),
                    "mtime_unix": mtime_unix,
                })
            })
            .collect();

        Ok(ToolOutput::structured(json!({
            "matches": structured_matches,
            "truncated": truncated,
            "search_root": search_root.to_string_lossy().into_owned(),
            "pattern": pattern,
            "text_fallback": out,
        })))
    }
}

/// Trailer line appended when the [`MAX_OUTPUT_BYTES`] guard forces us
/// to drop additional matches. Distinct wording from
/// [`TRUNCATION_NOTICE`] (which fires on the [`MAX_RESULTS`] count cap)
/// so callers can tell which guard fired.
fn byte_cap_trailer(dropped: usize) -> String {
    format!("... {dropped} more results truncated (output capped at ~100 KB)")
}

/// Glob metacharacters whose first appearance in `pattern` ends the
/// static prefix. `*`, `?`, `[`, and `{` are the four globset tokens that
/// can match more than the literal character itself; everything else is
/// treated as a literal path component.
const META_CHARS: &[char] = &['*', '?', '[', '{'];

/// Split an absolute `pattern` into a static walk root + the tail pattern
/// to match under it.
///
/// - Returns `None` for relative patterns (the caller falls back to cwd).
/// - With metacharacters: the longest leading run of glob-free path
///   components becomes the root; the rest, joined with `/`, is the
///   tail.
/// - Without metacharacters: the parent directory becomes the root and
///   the basename becomes the tail (a single-entry existence check).
///
/// Tilde expansion is handled upstream by `path::expand_path`; this
/// helper only sees already-resolved strings, so it does not re-implement
/// it. UNC roots are detected separately and rejected before this helper
/// is consulted.
pub(crate) fn absolute_pattern_split(pattern: &str) -> Option<(PathBuf, String)> {
    let p = Path::new(pattern);
    if !p.is_absolute() {
        return None;
    }

    let has_meta = pattern.contains(META_CHARS);

    if !has_meta {
        let parent = p.parent()?;
        let basename = p.file_name()?.to_string_lossy().into_owned();
        if basename.is_empty() {
            return None;
        }
        return Some((parent.to_path_buf(), basename));
    }

    let mut root = PathBuf::new();
    let mut tail: Vec<String> = Vec::new();
    let mut hit_meta = false;
    for comp in p.components() {
        let s = comp.as_os_str().to_string_lossy().into_owned();
        if !hit_meta && !s.contains(META_CHARS) {
            root.push(&s);
        } else {
            hit_meta = true;
            tail.push(s);
        }
    }

    if !hit_meta {
        return None;
    }
    // globset accepts `/` on every platform; joining the tail with `/`
    // keeps the matcher pattern portable even when components were parsed
    // from a backslash-separated Windows input.
    Some((root, tail.join("/")))
}

fn format_hit_path(path: &Path, cwd: &Path) -> String {
    crate::path::relativize_path(path, cwd)
}

/// True when `path` looks like a Windows UNC root (`\\server\share`). The
/// check is on the leading two backslashes only — verbatim variants
/// (`\\?\...`, `\\.\...`) share the same prefix and are also refused,
/// which is the cautious choice for a tool walking model-supplied paths.
#[cfg(windows)]
fn is_unc_path(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with(r"\\")
}

/// Maximum number of "did you mean" entries surfaced when the search
/// root does not exist. Three is the locked PRD ceiling: enough to
/// disambiguate a typo without flooding the error with noise.
pub(crate) const MAX_SUGGESTIONS: usize = 3;

/// Build the error message returned when the resolved search root does
/// not exist. Always names both the absolute path attempted and the
/// runner's cwd so the caller can disambiguate a wrong-cwd assumption
/// from a typo. When sibling suggestions are available a `did you mean:`
/// suffix is appended (up to [`MAX_SUGGESTIONS`] entries).
pub(crate) fn missing_root_message(search_root: &Path, cwd: &Path) -> String {
    let mut msg = format!(
        "path does not exist: {} (cwd: {})",
        search_root.display(),
        cwd.display()
    );
    let suggestions = suggest_siblings(search_root);
    if !suggestions.is_empty() {
        msg.push_str("; did you mean: ");
        msg.push_str(&suggestions.join(", "));
    }
    msg
}

/// Walk up from `target` to the deepest existing ancestor and return up
/// to [`MAX_SUGGESTIONS`] sibling basenames whose Levenshtein distance
/// (case-insensitive) to the missing child's basename is within a
/// length-aware threshold.
///
/// - Hidden / dotfile siblings ARE included; the walker default to
///   surface dotfiles (gap #4) carries through to suggestions.
/// - Permission-denied while listing the ancestor degrades to an empty
///   vec — the caller still gets a clean ENOENT, just without hints.
/// - Returns an empty vec when no ancestor exists (e.g. a stripped or
///   bare absolute root) or when no sibling clears the threshold.
pub(crate) fn suggest_siblings(target: &Path) -> Vec<String> {
    let mut current = target.to_path_buf();
    let (parent, missing_basename) = loop {
        let parent = match current.parent() {
            Some(p) => p.to_path_buf(),
            None => return Vec::new(),
        };
        let basename = match current.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return Vec::new(),
        };
        if parent.exists() {
            break (parent, basename);
        }
        current = parent;
    };

    let entries = match std::fs::read_dir(&parent) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };

    let target_lower = missing_basename.to_lowercase();
    let target_chars: Vec<char> = target_lower.chars().collect();
    let threshold = similarity_threshold(target_chars.len());

    let mut scored: Vec<(usize, String)> = Vec::new();
    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == missing_basename {
            continue;
        }
        let cand_lower = name.to_lowercase();
        let cand_chars: Vec<char> = cand_lower.chars().collect();
        let distance = levenshtein(&target_chars, &cand_chars);
        if distance <= threshold {
            scored.push((distance, name));
        }
    }

    // Closest first; lexicographic on ties so the output is deterministic.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.truncate(MAX_SUGGESTIONS);
    scored.into_iter().map(|(_, n)| n).collect()
}

/// Length-aware Levenshtein threshold for basename suggestions: short
/// names tolerate at most one edit (otherwise every 3-letter directory
/// looks "close" to every other), and longer names allow proportionally
/// more, capped so the suggestion list does not flood with weak matches
/// for very long names.
fn similarity_threshold(len: usize) -> usize {
    match len {
        0..=3 => 1,
        4..=8 => 2,
        9..=14 => 3,
        _ => 4,
    }
}

/// Standard Levenshtein distance between two char slices using a
/// rolling two-row DP. Operates on `char` so multi-byte UTF-8 names
/// (CJK, accented filenames) score by perceived edits rather than by
/// raw byte deltas.
fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Whether to honor `.gitignore` / `.ignore` / global excludes. Default ON;
/// the env var only flips OFF when set to the literal string `false`
/// (case-insensitive). Anything else — unset, empty, `true`, `1`, garbage —
/// keeps the default ON, so a typo can't silently disable filtering.
fn respect_gitignore() -> bool {
    match std::env::var(RESPECT_GITIGNORE_ENV) {
        Ok(v) => !v.eq_ignore_ascii_case("false"),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests;
