//! Description text shown to the model for the Grep tool.
//!
//! This wording is the model-facing contract for the search surface: tool
//! names referenced here (`Grep`, `Bash`, `Agent`) and the leading sentence
//! "A powerful search tool built on ripgrep" are what models are tuned
//! against, so edits should be deliberate. A drift-guard test in `tests.rs`
//! pins the description to this constant.

pub const DESCRIPTION: &str = "A powerful search tool built on ripgrep

  Usage:
  - ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` as a Bash command. The Grep tool has been optimized for correct permissions and access.
  - Supports full regex syntax (e.g., \"log.*Error\", \"function\\s+\\w+\")
  - Filter files with glob parameter (e.g., \"*.js\", \"**/*.tsx\") or type parameter (e.g., \"js\", \"py\", \"rust\"). The glob field accepts multiple patterns separated by whitespace or commas (e.g., \"*.ts *.tsx\" or \"*.ts,*.tsx\" or \"*.ts, *.tsx src/**/*.js\"). Brace expressions like \"*.{ts,tsx}\" are treated as a single token — the inner comma is not a separator.
  - Output modes: \"content\" shows matching lines, \"files_with_matches\" shows only file paths (default), \"count\" shows match counts
  - Use Agent tool for open-ended searches requiring multiple rounds
  - Pattern syntax: Uses ripgrep (not grep) - literal braces need escaping (use `interface\\{\\}` to find `interface{}` in Go code)
  - Multiline matching: By default patterns match within single lines only. For cross-line patterns like `struct \\{[\\s\\S]*?field`, use `multiline: true`
  - The `path` parameter accepts tilde-prefixed paths (e.g. `~/projects/foo`), relative paths (e.g. `./subdir`), and absolute paths. Omit `path` to search from the current working directory.
  - On Windows, UNC / network paths (starting with `\\` or `//`) are rejected.
  - Output paths are relative to the current working directory when the hit lies under it; otherwise they are absolute.
  - Very large result sets (above ~100 KB) are truncated at a line boundary; a `[truncated]` marker is appended when this occurs.
  - In `count` mode, results are sorted busiest-first (highest match count first); files with equal counts are ordered lexicographically by path.
  - In `count` mode, a summary line `Found N total occurrences across M files` is appended after the per-file list; it appears before the `[truncated]` marker when truncation also fires.
  - In `files_with_matches` mode, a `Found N files` summary line is prepended as the first line before the file list; it appears before the `[truncated]` marker when truncation also fires.
  - Dotfiles and hidden files (e.g. `.env.example`, `.github/workflows/*.yml`, `.eslintrc.*`) ARE searched by default.
  - VCS metadata directories (`.git`, `.svn`, `.hg`, `.bzr`, `.jj`, `.sl`) are always excluded from the search.
  - Pagination: the optional `offset` input field (default 0) skips the first N entries of the result list before applying `head_limit`. Use it only to resume a prior truncated result. Ordering is stable within a single search root across calls. When `head_limit` truncated the result OR a non-zero `offset` was supplied, a pagination footer `[paginated: limit=N offset=M]` is appended as the last line before any `[truncated]` byte-cap marker. Parse `limit` and `offset` from this footer to compute the next page's `offset` value.
  - In `content` mode, matched lines longer than 500 characters are reported as `[long line N chars]` instead of full content, where N is the original character count (Unicode scalar values). The `path:line_no:` prefix is still emitted so the hit location is known.
  - Searches time out after 30 seconds by default. A timed-out search returns a distinct recoverable error (`grep timed out after Ns`) so you can distinguish an incomplete search from one with no matches. Set the `LAUNCHPAD_GREP_TIMEOUT_SECS` environment variable (integer seconds; invalid or zero values fall back to the 30 s default) to adjust the limit when searching very large repos.
";
