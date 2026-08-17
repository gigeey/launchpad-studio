//! Description text shown to the model for the Glob tool.
//!
//! Two drift-guard tests in `tests.rs` keep this constant honest:
//! `description_returns_prompt_constant` asserts `Glob::description() ==
//! DESCRIPTION`, and `description_mentions_gitignore_env_var` asserts the
//! literal `LAUNCHPAD_GLOB_RESPECT_GITIGNORE` is present so a future rename
//! of the env var forces a matching prompt edit.

pub const DESCRIPTION: &str = "- Fast file pattern matching tool that works with any codebase size
- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"
- Brace expansion is supported: `src/{a,b}.rs` matches both `src/a.rs` and `src/b.rs`
- Pattern matching is case-sensitive: `*.RS` does NOT match `foo.rs`
- Negation prefixes (`!`) are NOT supported — write a positive pattern instead
- By default, gitignored files are hidden (`.gitignore`, `.ignore`, and global git excludes are honored). To include ignored files — e.g. when looking for a build artifact or a file under `target/` or `node_modules/` that an ordinary search misses — pass `no_ignore: true`. (An operator can also globally disable this via LAUNCHPAD_GLOB_RESPECT_GITIGNORE=false.)
- An absolute `pattern` (e.g. `/Users/foo/proj/**/*.rs`) is auto-split: the longest glob-free leading prefix becomes the search root and the rest is matched under it, so you can pass an absolute pattern without also setting `path`. If `path` is given alongside an absolute pattern, `path` wins and the absolute prefix is ignored.
- When the resolved search root does not exist, the error names the absolute path attempted AND the current working directory, plus up to 3 \"did you mean: ...\" sibling suggestions from the deepest existing ancestor. Use the cwd hint to disambiguate a wrong-directory assumption from a typo.
- Returns matching file paths sorted by modification time, newest first
- Hidden files and directories (e.g. `.env`, `.config/...`) are included by default
- Output is capped at ~1000 results and ~100 KB; when truncated, a marker line is appended
- Structured output is available: the response includes a `matches` array (each entry has `path` and `mtime_unix`), `truncated` bool, `search_root`, `pattern`, and a `text_fallback` field that is byte-identical to the plain-text rendering so text-only callers keep working unchanged
- Opt-in `offset` field (default 0): skips the first N entries of the sorted result list, for resuming a prior truncated result only. Ordering is stable within a single search root and a single call window. An oversized offset returns an empty result with `truncated: false`, not an error.
- Use this tool when you need to find files by name patterns
- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use the Agent tool instead";
