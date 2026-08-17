//! Unit tests for the Glob tool.
//!
//! Declared from `mod.rs` as `#[cfg(test)] mod tests;` 
//! per-tool folder layout — `tests.rs` is the same module as `mod.rs`, so
//! private items (constants, helpers) are in scope here.

use std::fs::{File, FileTimes};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use jsonschema::Validator;
use serde_json::json;
use tempfile::TempDir;

use super::{
    Glob, CANCELLED_MESSAGE, MAX_OUTPUT_BYTES, MAX_RESULTS, NO_RESULTS_MESSAGE,
    RESPECT_GITIGNORE_ENV, TRUNCATION_NOTICE,
};

fn ctx() -> RunnerContext {
    RunnerContext::new("sess", "agent").unwrap()
}

fn assert_recoverable_error(out: ToolOutput, contains: &str) {
    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable, "expected recoverable error, got fatal");
            assert!(
                message.contains(contains),
                "error message {message:?} did not contain {contains:?}"
            );
        }
        other => panic!("expected ToolOutput::Error, got {other:?}"),
    }
}

/// Extract the text representation from a successful ToolOutput.
/// For Structured outputs, reads the `text_fallback` field which is
/// byte-identical to the old Text rendering.
fn text_of(out: ToolOutput) -> String {
    match out {
        ToolOutput::Structured(v) => v["text_fallback"]
            .as_str()
            .expect("structured output must have a text_fallback string field")
            .to_string(),
        ToolOutput::Text(s) => s,
        other => panic!("expected text-like ToolOutput, got {other:?}"),
    }
}

/// Extract the structured JSON value from a Structured ToolOutput.
fn structured_of(out: ToolOutput) -> serde_json::Value {
    match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected ToolOutput::Structured, got {other:?}"),
    }
}

/// Set the mtime of `path` to `t`. We open with write access because
/// `File::set_times` requires write permission on Unix.
fn set_mtime(path: &std::path::Path, t: SystemTime) {
    let f = File::options().write(true).open(path).unwrap();
    let times = FileTimes::new().set_modified(t);
    f.set_times(times).unwrap();
}

/// RAII scope guard for a single env var. Captures the prior value on
/// construction and restores (or removes) it on drop, so a panic in the
/// test body still leaves `$LAUNCHPAD_GLOB_RESPECT_GITIGNORE` (or any other
/// captured key) in its original state. Pair with `#[serial]` when other
/// tests are sensitive to the same key — the guard isolates state across
/// drops, but `set_var` itself is process-global until then.
struct EnvGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, prior }
    }

    fn unset(key: &'static str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn name_and_concurrency_safe() {
    let g = Glob;
    assert_eq!(g.name(), "Glob");
    assert!(g.is_concurrency_safe());
}

#[test]
fn description_returns_prompt_constant() {
    let g = Glob;
    assert_eq!(g.description(), super::prompt::DESCRIPTION);
    assert!(!g.description().is_empty());
}

#[test]
fn description_mentions_gitignore_env_var() {
    // Drift guard: every env knob the tool actually reads must be named in
    // the prompt. This pins the only env var Glob reads
    // (`LAUNCHPAD_GLOB_RESPECT_GITIGNORE`) so a future rename of the const
    // would also have to touch `prompt::DESCRIPTION` to keep this test
    // green — the prompt cannot silently drift from the implementation.
    assert!(
        super::prompt::DESCRIPTION.contains(RESPECT_GITIGNORE_ENV),
        "prompt::DESCRIPTION must mention {RESPECT_GITIGNORE_ENV} verbatim",
    );
}

#[test]
fn input_schema_is_self_contained_and_valid() {
    let schema = Glob.input_schema();
    let validator = Validator::new(&schema).expect("schema must compile");

    let good = json!({"pattern": "**/*.rs", "path": "/tmp"});
    assert!(validator.is_valid(&good));

    let pattern_only = json!({"pattern": "*.txt"});
    assert!(validator.is_valid(&pattern_only));

    let missing_required = json!({"path": "/tmp"});
    assert!(!validator.is_valid(&missing_required));

    let wrong_type = json!({"pattern": 5});
    assert!(!validator.is_valid(&wrong_type));
}

#[tokio::test]
async fn happy_path_returns_absolute_paths_sorted_by_mtime_desc() {
    let dir = TempDir::new().unwrap();
    tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();

    let old = dir.path().join("sub").join("old.txt");
    let mid = dir.path().join("mid.txt");
    let new = dir.path().join("sub").join("new.txt");
    tokio::fs::write(&old, "o").await.unwrap();
    tokio::fs::write(&mid, "m").await.unwrap();
    tokio::fs::write(&new, "n").await.unwrap();

    // Set mtimes far enough apart that filesystem timestamp granularity
    // can't collapse them.
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&old, t0);
    set_mtime(&mid, t0 + Duration::from_secs(60));
    set_mtime(&new, t0 + Duration::from_secs(120));

    let out = Glob
        .invoke(
            json!({"pattern": "**/*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "got {text:?}");
    // Newest first.
    assert_eq!(lines[0], new.to_str().unwrap());
    assert_eq!(lines[1], mid.to_str().unwrap());
    assert_eq!(lines[2], old.to_str().unwrap());
    // Absolute paths only.
    for line in &lines {
        assert!(
            line.starts_with('/'),
            "expected absolute path, got {line:?}"
        );
    }
}

#[tokio::test]
async fn defaults_path_to_cwd_when_omitted() {
    // `cargo test` runs with cwd set to the crate root, which contains
    // `Cargo.toml`. Globbing for it without a `path` argument exercises the
    // current_dir() default.
    let out = Glob
        .invoke(json!({"pattern": "Cargo.toml"}), &ctx())
        .await
        .unwrap();
    let text = text_of(out);
    assert!(
        text.contains("Cargo.toml"),
        "expected Cargo.toml hit when path omitted, got {text:?}"
    );
}

#[tokio::test]
#[serial_test::serial(launchpad_glob_gitignore_env)]
async fn respects_gitignore_excluding_matched_files() {
    // Serialized with the env-var tests because they mutate the
    // process-global `LAUNCHPAD_GLOB_RESPECT_GITIGNORE`. Without the
    // `#[serial]` tag a parallel bypass test could flip the env var while
    // this test is mid-walk and silently surface `secret.txt`.
    let _guard = EnvGuard::unset(RESPECT_GITIGNORE_ENV);
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "secret.txt\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("kept.txt"), "k")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("secret.txt"), "s")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains("kept.txt"),
        "kept.txt should be present: {text:?}"
    );
    assert!(
        !text.contains("secret.txt"),
        "secret.txt is gitignored and must not appear: {text:?}"
    );
}

#[tokio::test]
async fn non_existent_path_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("does-not-exist");
    let s = missing.to_str().unwrap().to_string();

    let out = Glob
        .invoke(json!({"pattern": "*", "path": s.clone()}), &ctx())
        .await
        .unwrap();
    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable);
            assert!(message.contains(&s), "error must name path: {message:?}");
            assert!(message.contains("does not exist"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_pattern_returns_recoverable_error_with_parse_msg() {
    let dir = TempDir::new().unwrap();
    // Unclosed character class — globset rejects this at compile time.
    let out = Glob
        .invoke(
            json!({"pattern": "[abc", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "invalid glob pattern");
}

#[tokio::test]
async fn one_thousand_result_cap_is_enforced_with_trailer() {
    let dir = TempDir::new().unwrap();
    // A flat directory of MAX_RESULTS + 50 files. globset's `*` does not
    // cross path separators, so single-component names are correct here.
    let total = MAX_RESULTS + 50;
    for i in 0..total {
        let p = dir.path().join(format!("file_{i:05}.txt"));
        tokio::fs::write(&p, "x").await.unwrap();
    }

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    let lines: Vec<&str> = text.lines().collect();
    // MAX_RESULTS path lines + 1 trailer line.
    assert_eq!(
        lines.len(),
        MAX_RESULTS + 1,
        "expected {} path lines + trailer, got {}",
        MAX_RESULTS,
        lines.len()
    );
    assert_eq!(lines.last().copied(), Some(TRUNCATION_NOTICE));
}

#[tokio::test]
async fn output_capped_at_byte_budget_with_truncation_marker() {
    let dir = TempDir::new().unwrap();
    // Long-name files are sized so the byte budget trips well before
    // MAX_RESULTS does — keeps this test scoped to the byte-cap branch
    // even though both guards are layered.
    let pad = "p".repeat(220);
    let total = 500;
    for i in 0..total {
        let name = format!("file_{i:05}_{pad}.txt");
        tokio::fs::write(dir.path().join(&name), "x").await.unwrap();
    }

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.ends_with(" more results truncated (output capped at ~100 KB)"),
        "expected byte-cap trailer at end of output: ...{:?}",
        &text[text.len().saturating_sub(120)..]
    );
    assert!(
        !text.contains(TRUNCATION_NOTICE),
        "MAX_RESULTS notice must not fire when byte cap drops paths first: {text:?}"
    );

    let len = text.len();
    let cap = MAX_OUTPUT_BYTES;
    let upper = cap + cap / 20; // within ~5% of the cap
    assert!(
        len <= upper,
        "output ({len} bytes) must stay within ~5% of cap ({cap} bytes)"
    );
    // Sanity: the cap should genuinely be approached. If we were
    // dropping far too aggressively this lower bound would catch it.
    let lower = cap * 4 / 5;
    assert!(
        len >= lower,
        "output ({len} bytes) is well under the cap ({cap} bytes); test inputs may be undersized"
    );
}

#[tokio::test]
async fn hidden_files_are_included_by_default() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".hidden_file"), "h")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("visible.txt"), "v")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "**/.*", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains(".hidden_file"),
        ".hidden_file must be returned: {text:?}"
    );
    assert!(
        !text.contains("visible.txt"),
        "non-hidden file should not match `**/.*`: {text:?}"
    );
}

#[tokio::test]
async fn results_are_sorted_newest_first_by_mtime() {
    // Regression guard for the locked newest-first contract documented in
    // `prompt::DESCRIPTION`. A future `WalkBuilder` flag flip or sort-order
    // edit would silently invert results without this test.
    let dir = TempDir::new().unwrap();
    let oldest = dir.path().join("oldest.txt");
    let middle = dir.path().join("middle.txt");
    let newest = dir.path().join("newest.txt");
    tokio::fs::write(&oldest, "o").await.unwrap();
    tokio::fs::write(&middle, "m").await.unwrap();
    tokio::fs::write(&newest, "n").await.unwrap();

    // Stagger mtimes by 60s so filesystem timestamp granularity cannot
    // collapse two of them onto the same instant.
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&oldest, t0);
    set_mtime(&middle, t0 + Duration::from_secs(60));
    set_mtime(&newest, t0 + Duration::from_secs(120));

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 hits, got {text:?}");
    assert_eq!(lines[0], newest.to_str().unwrap(), "newest must be first");
    assert_eq!(lines[1], middle.to_str().unwrap(), "middle must be second");
    assert_eq!(lines[2], oldest.to_str().unwrap(), "oldest must be last");
}

#[tokio::test]
async fn no_matches_returns_no_files_found() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "x")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);
    assert_eq!(text, NO_RESULTS_MESSAGE);
}

#[tokio::test]
async fn cancellation_returns_cancelled_within_100ms() {
    let dir = TempDir::new().unwrap();
    // A small population is fine — the cancel poll fires on the very first
    // iteration (i == 0), so a pre-cancelled token returns immediately.
    for i in 0..10 {
        tokio::fs::write(dir.path().join(format!("f_{i}.txt")), "x")
            .await
            .unwrap();
    }

    let context = ctx();
    context.cancel.cancel();

    let path_s = dir.path().to_str().unwrap().to_string();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        Glob.invoke(json!({"pattern": "*.txt", "path": path_s}), &context),
    )
    .await
    .expect("must return within 100ms");

    match result {
        Err(AoError::Internal(msg)) => assert_eq!(msg, CANCELLED_MESSAGE),
        other => panic!("expected AoError::Internal(\"cancelled\"), got {other:?}"),
    }
}

#[test]
fn format_hit_path_relativizes_under_cwd_keeps_others_absolute() {
    // Two real tempdir trees: one acts as the synthetic cwd (contains an
    // "inside" hit); the other lives outside it (the "outside" hit).
    let cwd_dir = TempDir::new().unwrap();
    let outside_dir = TempDir::new().unwrap();
    let cwd = cwd_dir.path();

    let inside = cwd.join("sub").join("kept.txt");
    std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
    std::fs::write(&inside, "x").unwrap();

    let outside = outside_dir.path().join("away.txt");
    std::fs::write(&outside, "y").unwrap();

    let inside_expected = PathBuf::from("sub").join("kept.txt");
    assert_eq!(
        super::format_hit_path(&inside, cwd),
        inside_expected.to_string_lossy().into_owned(),
    );

    assert_eq!(
        super::format_hit_path(&outside, cwd),
        outside.to_string_lossy().into_owned(),
    );
}

#[tokio::test]
async fn brace_expansion_matches_alternatives() {
    // Pins globset's brace-expansion behavior. `src/{a,b}.rs` must match
    // both `src/a.rs` and `src/b.rs` and NOT `src/c.rs`.
    let dir = TempDir::new().unwrap();
    tokio::fs::create_dir(dir.path().join("src")).await.unwrap();
    tokio::fs::write(dir.path().join("src").join("a.rs"), "a")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("src").join("b.rs"), "b")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("src").join("c.rs"), "c")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "src/{a,b}.rs", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(text.contains("a.rs"), "src/a.rs must match: {text:?}");
    assert!(text.contains("b.rs"), "src/b.rs must match: {text:?}");
    assert!(
        !text.contains("c.rs"),
        "src/c.rs must NOT match the brace alternation: {text:?}"
    );
}

#[tokio::test]
async fn leading_bang_is_literal_not_negation() {
    // Pins the "negation NOT supported" decision. globset's default mode
    // treats `!` as a literal character, so `!*.rs` is a positive pattern
    // matching paths whose basename starts with `!` and ends in `.rs`.
    // We assert that the bang-prefixed file IS returned and the plain
    // `.rs` file is NOT — proving `!` was not interpreted as a negation
    // (which would have inverted the result).
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("foo.rs"), "f")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("!foo.rs"), "b")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "!*.rs", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains("!foo.rs"),
        "literal `!` prefix must match: {text:?}"
    );
    // Be precise: `foo.rs` (no bang) appears as a substring of `!foo.rs`,
    // so check line-by-line for an exact match instead of `.contains`.
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        !lines
            .iter()
            .any(|l| l.ends_with("/foo.rs") || *l == "foo.rs"),
        "bare foo.rs must NOT match `!*.rs` under literal interpretation: {text:?}"
    );
}

#[tokio::test]
async fn matching_is_case_sensitive_by_default() {
    // Pins case-sensitive default. `*.RS` (uppercase) must NOT match a
    // file named `foo.rs` (lowercase). globset's default is
    // `case_insensitive: false`, so we rely on that default — this test
    // catches a future flag flip.
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("foo.rs"), "x")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.RS", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);
    assert_eq!(
        text, NO_RESULTS_MESSAGE,
        "case-sensitive matcher must not return foo.rs for *.RS: {text:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_to_outside_root_are_not_followed() {
    // Pins the WalkBuilder default of `follow_links(false)`. A future edit
    // that flips that flag would let the walker descend through symlinks
    // pointing outside the search root and silently surface paths the
    // caller never asked for — so we assert here that a symlinked-out
    // marker file is NOT reachable while a normal in-root file IS.
    //
    // Symlink creation is gated to `cfg(unix)` because Windows requires
    // either Developer Mode or `SeCreateSymbolicLinkPrivilege`, neither of
    // which we can assume in a CI runner. Skipping cleanly on non-unix
    // beats a flakey `permission denied` from `symlink_dir`.
    let outside_dir = TempDir::new().unwrap();
    let outside_marker = outside_dir.path().join("outside_secret.txt");
    tokio::fs::write(&outside_marker, "secret").await.unwrap();

    let search_dir = TempDir::new().unwrap();
    let inside_file = search_dir.path().join("inside.txt");
    tokio::fs::write(&inside_file, "inside").await.unwrap();

    let link_path = search_dir.path().join("link_to_outside");
    std::os::unix::fs::symlink(outside_dir.path(), &link_path).unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "**/*.txt", "path": search_dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains("inside.txt"),
        "inside.txt must be returned: {text:?}"
    );
    assert!(
        !text.contains("outside_secret.txt"),
        "walker must not follow symlink into outside dir: {text:?}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn unc_paths_are_rejected_before_stat_on_windows() {
    // The host is bogus on purpose: if the early UNC check is missing the
    // tokio::fs::metadata call would attempt an SMB/NTLM handshake against
    // it, which is exactly the leak this story prevents. The recoverable
    // error message proves the early-return fired before any stat.
    let out = Glob
        .invoke(
            json!({
                "pattern": "*",
                "path": r"\\nonexistent-host-launchpad-glob-test\share",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "UNC paths are not supported");
}

#[tokio::test]
#[serial_test::serial(launchpad_glob_gitignore_env)]
async fn gitignore_respected_by_default_when_env_var_unset() {
    // Pins the locked decision: gitignore filtering is ON by default and
    // does NOT require the env var to be set to any specific value. We
    // explicitly remove the env var here so a CI runner that happens to
    // export `LAUNCHPAD_GLOB_RESPECT_GITIGNORE=true` (or anything else)
    // can't mask a regression where the default flips to OFF.
    let _guard = EnvGuard::unset(RESPECT_GITIGNORE_ENV);

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "ignored.txt\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("kept.txt"), "k")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ignored.txt"), "i")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains("kept.txt"),
        "kept.txt should be present: {text:?}"
    );
    assert!(
        !text.contains("ignored.txt"),
        "ignored.txt should be filtered by gitignore under default: {text:?}"
    );
}

#[tokio::test]
#[serial_test::serial(launchpad_glob_gitignore_env)]
async fn gitignore_bypassed_when_env_var_is_false() {
    // Pins the env-var opt-out. With `LAUNCHPAD_GLOB_RESPECT_GITIGNORE=false`
    // the same `.gitignore` rule must NOT filter the matching file.
    // `EnvGuard` restores the prior value on drop so a panicked assert
    // doesn't leak state into the next test.
    let _guard = EnvGuard::set(RESPECT_GITIGNORE_ENV, "false");

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "ignored.txt\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("kept.txt"), "k")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ignored.txt"), "i")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains("kept.txt"),
        "kept.txt must be present: {text:?}"
    );
    assert!(
        text.contains("ignored.txt"),
        "ignored.txt MUST be returned when env var bypass is set: {text:?}"
    );
}

#[tokio::test]
#[serial_test::serial(launchpad_glob_gitignore_env)]
async fn gitignore_bypassed_when_no_ignore_param_true() {
    // The model-facing opt-out: with the env var unset (default = respect),
    // passing `no_ignore: true` on the call must surface the gitignored file.
    // This is the per-call counterpart to the global env-var bypass.
    let _guard = EnvGuard::unset(RESPECT_GITIGNORE_ENV);

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "ignored.txt\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("kept.txt"), "k")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ignored.txt"), "i")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap(), "no_ignore": true}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains("kept.txt"),
        "kept.txt must be present: {text:?}"
    );
    assert!(
        text.contains("ignored.txt"),
        "ignored.txt MUST be returned when no_ignore:true is passed: {text:?}"
    );
}

#[tokio::test]
#[serial_test::serial(launchpad_glob_gitignore_env)]
async fn no_ignore_string_true_is_coerced() {
    // Providers that serialize booleans as strings must still trigger the
    // bypass: `no_ignore: "true"` is coerced the same as the native bool.
    let _guard = EnvGuard::unset(RESPECT_GITIGNORE_ENV);

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "ignored.txt\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ignored.txt"), "i")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap(), "no_ignore": "true"}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        text.contains("ignored.txt"),
        "ignored.txt MUST be returned when no_ignore:\"true\" (string) is passed: {text:?}"
    );
}

#[tokio::test]
#[serial_test::serial(launchpad_glob_gitignore_env)]
async fn no_ignore_false_still_respects_gitignore() {
    // The explicit default: `no_ignore: false` (env unset) keeps filtering ON,
    // so an unrecognized/false value can't silently disable ignore handling.
    let _guard = EnvGuard::unset(RESPECT_GITIGNORE_ENV);

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "ignored.txt\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ignored.txt"), "i")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap(), "no_ignore": false}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);

    assert!(
        !text.contains("ignored.txt"),
        "ignored.txt should stay filtered when no_ignore:false: {text:?}"
    );
}

#[test]
fn absolute_pattern_split_handles_meta_and_literal() {
    use std::path::PathBuf;

    // With metacharacters: longest static prefix becomes the root, the
    // tail keeps the meta segments joined with `/`.
    let (root, tail) = super::absolute_pattern_split("/Users/foo/proj/**/*.rs").unwrap();
    assert_eq!(root, PathBuf::from("/Users/foo/proj"));
    assert_eq!(tail, "**/*.rs");

    // Purely literal: parent + basename.
    let (root, tail) = super::absolute_pattern_split("/etc/hosts").unwrap();
    assert_eq!(root, PathBuf::from("/etc"));
    assert_eq!(tail, "hosts");

    // Meta in the very first component below root.
    let (root, tail) = super::absolute_pattern_split("/{a,b}.rs").unwrap();
    assert_eq!(root, PathBuf::from("/"));
    assert_eq!(tail, "{a,b}.rs");

    // Relative patterns are not split.
    assert!(super::absolute_pattern_split("**/*.rs").is_none());
    assert!(super::absolute_pattern_split("src/foo.rs").is_none());
}

#[tokio::test]
async fn absolute_pattern_with_metacharacters_auto_splits_when_path_omitted() {
    let dir = TempDir::new().unwrap();
    tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();
    let target = dir.path().join("sub").join("hit.rs");
    tokio::fs::write(&target, "x").await.unwrap();
    tokio::fs::write(dir.path().join("skip.txt"), "y")
        .await
        .unwrap();

    // The pattern is absolute and contains `**/*.rs`. With no `path` arg,
    // the search root must be peeled out of the static prefix (the
    // tempdir) and the tail (`**/*.rs`) used as the matcher.
    let pattern = format!("{}/**/*.rs", dir.path().to_str().unwrap());
    let out = Glob
        .invoke(json!({ "pattern": pattern }), &ctx())
        .await
        .unwrap();
    let text = text_of(out);
    assert!(
        text.contains("hit.rs"),
        "absolute-prefix walk must surface hit.rs: {text:?}"
    );
    assert!(
        !text.contains("skip.txt"),
        "tail pattern `**/*.rs` must filter non-rs files: {text:?}"
    );
}

#[tokio::test]
async fn absolute_pattern_without_metacharacters_is_existence_check() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("only.txt");
    tokio::fs::write(&target, "x").await.unwrap();

    // A purely literal absolute pattern: root = parent, tail = basename.
    // The walker should surface exactly one entry, the named file.
    let pattern = target.to_str().unwrap().to_string();
    let out = Glob
        .invoke(json!({ "pattern": pattern }), &ctx())
        .await
        .unwrap();
    let text = text_of(out);

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "expected single hit, got {text:?}");
    assert_eq!(lines[0], target.to_str().unwrap());
}

#[tokio::test]
async fn explicit_path_wins_over_absolute_pattern_prefix() {
    // Two tempdirs; the one named in `path` is the only one we should
    // walk. The absolute prefix in `pattern` belongs to a different dir
    // and must be ignored (no error).
    let absolute_dir = TempDir::new().unwrap();
    let absolute_target = absolute_dir.path().join("absolute_only.txt");
    tokio::fs::write(&absolute_target, "a").await.unwrap();

    let path_dir = TempDir::new().unwrap();
    let path_target = path_dir.path().join("path_only.txt");
    tokio::fs::write(&path_target, "p").await.unwrap();

    // The pattern's absolute prefix points at `absolute_dir`, but `path`
    // points at `path_dir`. Per the locked decision, `path` wins.
    let pattern = format!("{}/**/*.txt", absolute_dir.path().to_str().unwrap());
    let out = Glob
        .invoke(
            json!({"pattern": pattern, "path": path_dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let text = text_of(out);
    assert!(
        text.contains("path_only.txt"),
        "explicit path's file must surface: {text:?}"
    );
    assert!(
        !text.contains("absolute_only.txt"),
        "absolute-prefix dir must be ignored when explicit path is given: {text:?}"
    );
}

#[tokio::test]
async fn static_prefix_peeling_stops_at_brace() {
    // `{a,b}.rs` is a glob metacharacter — the static prefix must end at
    // the parent of the brace component, not include it. We assert this
    // by writing `a.rs` and `c.rs`: the brace alternation matches `a.rs`
    // and not `c.rs`, which can only happen if the brace was passed to
    // the matcher rather than treated as a literal directory name.
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("src");
    tokio::fs::create_dir(&sub).await.unwrap();
    tokio::fs::write(sub.join("a.rs"), "a").await.unwrap();
    tokio::fs::write(sub.join("b.rs"), "b").await.unwrap();
    tokio::fs::write(sub.join("c.rs"), "c").await.unwrap();

    let pattern = format!("{}/{{a,b}}.rs", sub.to_str().unwrap());
    let out = Glob
        .invoke(json!({ "pattern": pattern }), &ctx())
        .await
        .unwrap();
    let text = text_of(out);
    assert!(text.contains("a.rs"), "a.rs must match: {text:?}");
    assert!(text.contains("b.rs"), "b.rs must match: {text:?}");
    assert!(
        !text.contains("c.rs"),
        "brace alternation must not match c.rs: {text:?}"
    );
}

#[cfg(windows)]
#[test]
fn absolute_pattern_split_handles_windows_drive_paths() {
    use std::path::PathBuf;

    let (root, tail) = super::absolute_pattern_split(r"C:\Users\foo\proj\**\*.rs").unwrap();
    assert_eq!(root, PathBuf::from(r"C:\Users\foo\proj"));
    assert_eq!(tail, "**/*.rs");

    let (root, tail) = super::absolute_pattern_split(r"C:\Windows\notepad.exe").unwrap();
    assert_eq!(root, PathBuf::from(r"C:\Windows"));
    assert_eq!(tail, "notepad.exe");
}

#[tokio::test]
async fn missing_root_message_includes_cwd_and_path() {
    // The ENOENT error must name both the resolved absolute path and the
    // runner's cwd so the model can disambiguate a wrong-cwd assumption
    // from a typo. Suggestions are absent when the missing leaf has no
    // close-enough siblings (the empty tempdir below).
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("absolutely-no-such-file");
    let cwd = std::env::current_dir().unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*", "path": missing.to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable);
            assert!(
                message.contains("does not exist"),
                "must keep ENOENT marker: {message:?}"
            );
            assert!(
                message.contains(missing.to_str().unwrap()),
                "must name the attempted absolute path: {message:?}"
            );
            assert!(
                message.contains(cwd.to_str().unwrap()),
                "must include cwd hint: {message:?}"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn missing_root_surfaces_did_you_mean_for_near_miss() {
    // Near-miss case: a typo'd basename next to a real sibling. The
    // suggestion list must include the real name so the caller can
    // recover with one retry. Hidden siblings are kept as candidates
    // (gap #4 default), so we seed a dotfile too and confirm it lands
    // in the candidate set when it's the closest match.
    let dir = TempDir::new().unwrap();
    tokio::fs::create_dir(dir.path().join("docs"))
        .await
        .unwrap();
    tokio::fs::create_dir(dir.path().join(".docs_hidden"))
        .await
        .unwrap();
    tokio::fs::create_dir(dir.path().join("totally_unrelated"))
        .await
        .unwrap();

    let typo = dir.path().join("docz"); // 1 edit from "docs"
    let out = Glob
        .invoke(
            json!({"pattern": "*", "path": typo.to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let message = match out {
        ToolOutput::Error { message, .. } => message,
        other => panic!("expected Error, got {other:?}"),
    };

    assert!(
        message.contains("did you mean"),
        "near-miss must trigger suggestion: {message:?}"
    );
    assert!(
        message.contains("docs"),
        "closest sibling 'docs' must be suggested: {message:?}"
    );
    assert!(
        !message.contains("totally_unrelated"),
        "far-miss must be filtered by threshold: {message:?}"
    );
}

#[tokio::test]
async fn missing_root_caps_suggestions_at_three() {
    // Pin the locked decision: at most three "did you mean" entries.
    // We seed five siblings within the length-aware threshold of
    // "alphr" (len 5 → threshold 2) so the cap is what trims the list,
    // not the threshold filter.
    let dir = TempDir::new().unwrap();
    for name in ["alpha", "alph", "alphx", "alphz", "alpa"] {
        tokio::fs::create_dir(dir.path().join(name)).await.unwrap();
    }

    let typo = dir.path().join("alphr");
    let out = Glob
        .invoke(
            json!({"pattern": "*", "path": typo.to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let message = match out {
        ToolOutput::Error { message, .. } => message,
        other => panic!("expected Error, got {other:?}"),
    };

    let suffix = match message.split_once("did you mean: ") {
        Some((_, rest)) => rest.to_string(),
        None => panic!("expected suggestion suffix in: {message:?}"),
    };
    let suggestions: Vec<&str> = suffix.split(", ").collect();
    assert_eq!(
        suggestions.len(),
        super::MAX_SUGGESTIONS,
        "expected exactly {} suggestions, got {:?}",
        super::MAX_SUGGESTIONS,
        suggestions
    );
}

#[tokio::test]
async fn missing_root_walks_up_when_parent_missing() {
    // Multiple missing levels: only the grandparent exists. The
    // suggestion must come from siblings of the FIRST missing component
    // below the grandparent (here, "missing_branch"), not from the
    // grandparent itself. We pin this by placing a near-miss sibling
    // ("missing_brunch") next to the missing branch.
    let dir = TempDir::new().unwrap();
    tokio::fs::create_dir(dir.path().join("missing_brunch"))
        .await
        .unwrap();

    // Two-deep miss: dir/missing_branch/leaf where missing_branch itself
    // does not exist.
    let target = dir.path().join("missing_branch").join("leaf");
    let out = Glob
        .invoke(
            json!({"pattern": "*", "path": target.to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let message = match out {
        ToolOutput::Error { message, .. } => message,
        other => panic!("expected Error, got {other:?}"),
    };

    assert!(
        message.contains("missing_brunch"),
        "walk-up must surface grandparent's siblings of the missing branch: {message:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn missing_root_degrades_when_parent_listing_denied() {
    // Permission-denied while listing the existing ancestor must
    // degrade to ENOENT-without-suggestions, not bubble up an error.
    // Setup: parent dir has exec but no read (`chmod 0o111`), so
    // `metadata()` on a missing child still returns ENOENT but
    // `read_dir` of the parent returns EACCES. We restore perms before
    // TempDir drop so cleanup succeeds.
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let parent = dir.path().join("locked");
    tokio::fs::create_dir(&parent).await.unwrap();
    tokio::fs::write(parent.join("near_miss.txt"), "x")
        .await
        .unwrap();

    let mut perms = std::fs::metadata(&parent).unwrap().permissions();
    perms.set_mode(0o111); // exec only, no read
    std::fs::set_permissions(&parent, perms).unwrap();

    let missing = parent.join("near_misz.txt");
    let out = Glob
        .invoke(
            json!({"pattern": "*", "path": missing.to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();

    // Restore perms so TempDir cleanup works regardless of test outcome.
    let mut perms = std::fs::metadata(&parent).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&parent, perms).unwrap();

    let message = match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable);
            message
        }
        other => panic!("expected Error, got {other:?}"),
    };
    assert!(
        message.contains("does not exist"),
        "must still surface ENOENT: {message:?}"
    );
    assert!(
        !message.contains("did you mean"),
        "permission-denied listing must degrade to no-suggestions: {message:?}"
    );
}

#[test]
fn levenshtein_handles_empty_and_unicode() {
    use super::levenshtein;

    let empty: Vec<char> = Vec::new();
    let foo: Vec<char> = "foo".chars().collect();
    assert_eq!(levenshtein(&empty, &foo), 3);
    assert_eq!(levenshtein(&foo, &empty), 3);
    assert_eq!(levenshtein(&foo, &foo), 0);

    let kitten: Vec<char> = "kitten".chars().collect();
    let sitting: Vec<char> = "sitting".chars().collect();
    assert_eq!(levenshtein(&kitten, &sitting), 3);

    // Multi-byte chars score by char delta, not byte delta.
    let cafe_e: Vec<char> = "café".chars().collect();
    let cafe_a: Vec<char> = "cafa".chars().collect();
    assert_eq!(levenshtein(&cafe_e, &cafe_a), 1);
}

#[tokio::test]
async fn dispatches_via_registry() {
    use ao_engine_tools_core::Registry;

    let dir = TempDir::new().unwrap();
    let p = dir.path().join("hello.txt");
    tokio::fs::write(&p, "hi").await.unwrap();

    let mut r = Registry::new();
    r.register_io(Arc::new(Glob));
    let context = RunnerContext::new("s", "a")
        .unwrap()
        .with_registry(Arc::new(r));

    let tool = context.registry.lookup_io("Glob").expect("registered");
    let out = tool
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &context,
        )
        .await
        .unwrap();
    let text = text_of(out);
    assert!(
        text.contains("hello.txt"),
        "expected hello.txt in output: {text:?}"
    );
}

// ── Structured output tests ─────────────────────────────────────────────────

#[tokio::test]
async fn structured_output_has_documented_shape() {
    // Verify the JSON object has all required fields with the right types:
    // matches array (path/mtime_unix), truncated bool, search_root string,
    // pattern string, text_fallback string.
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.rs"), "x")
        .await
        .unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    assert!(v["matches"].is_array(), "matches must be an array");
    assert!(v["truncated"].is_boolean(), "truncated must be a bool");
    assert!(v["search_root"].is_string(), "search_root must be a string");
    assert!(v["pattern"].is_string(), "pattern must be a string");
    assert!(
        v["text_fallback"].is_string(),
        "text_fallback must be a string"
    );

    let matches = v["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "expected one match");
    let m = &matches[0];
    assert!(m["path"].is_string(), "match.path must be a string");
    assert!(
        m["mtime_unix"].is_i64() || m["mtime_unix"].is_u64(),
        "match.mtime_unix must be an integer"
    );
    assert!(
        m["path"].as_str().unwrap().contains("a.rs"),
        "match path must contain a.rs"
    );

    assert_eq!(
        v["search_root"].as_str().unwrap(),
        dir.path().to_str().unwrap(),
        "search_root must be the resolved absolute dir path"
    );
    assert_eq!(v["pattern"].as_str().unwrap(), "*.rs");
    assert_eq!(v["truncated"].as_bool().unwrap(), false);
}

#[tokio::test]
async fn structured_text_fallback_matches_old_text_rendering() {
    // text_fallback must be byte-identical to what the tool returned as Text
    // before structured output was introduced: newline-separated paths
    // (relative-where-possible), newest first, no trailing newline.
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("a.rs");
    let b = dir.path().join("b.rs");
    tokio::fs::write(&a, "a").await.unwrap();
    tokio::fs::write(&b, "b").await.unwrap();

    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&a, t0);
    set_mtime(&b, t0 + Duration::from_secs(60));

    let out = Glob
        .invoke(
            json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    let fallback = v["text_fallback"].as_str().unwrap();
    let lines: Vec<&str> = fallback.lines().collect();
    assert_eq!(lines.len(), 2, "two files must appear in text_fallback");
    // b.rs has the later mtime — must come first.
    assert!(
        lines[0].contains("b.rs"),
        "newer b.rs must be first in text_fallback: {fallback:?}"
    );
    assert!(
        lines[1].contains("a.rs"),
        "older a.rs must be second in text_fallback: {fallback:?}"
    );

    // text_fallback must also match what we'd get from matches[].path in order.
    let match_paths: Vec<&str> = v["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        match_paths, lines,
        "text_fallback lines must match matches[].path order"
    );
}

#[tokio::test]
async fn empty_result_structured_output() {
    // Empty match set: matches: [], truncated: false,
    // text_fallback == NO_RESULTS_MESSAGE.
    let dir = TempDir::new().unwrap();

    let out = Glob
        .invoke(
            json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    assert!(
        v["matches"].as_array().unwrap().is_empty(),
        "empty dir must yield matches: []"
    );
    assert_eq!(v["truncated"].as_bool().unwrap(), false);
    assert_eq!(v["text_fallback"].as_str().unwrap(), NO_RESULTS_MESSAGE);
}

#[tokio::test]
async fn truncated_flag_flips_on_max_results_cap() {
    // truncated must be true when MAX_RESULTS fires.
    let dir = TempDir::new().unwrap();
    let total = MAX_RESULTS + 1;
    for i in 0..total {
        tokio::fs::write(dir.path().join(format!("f_{i:05}.txt")), "x")
            .await
            .unwrap();
    }

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    assert_eq!(
        v["truncated"].as_bool().unwrap(),
        true,
        "truncated must be true when MAX_RESULTS cap fires"
    );
    assert_eq!(
        v["matches"].as_array().unwrap().len(),
        MAX_RESULTS,
        "matches must be capped at MAX_RESULTS"
    );
}

#[tokio::test]
async fn truncated_flag_flips_on_byte_budget_cap() {
    // truncated must be true when the byte-budget guard fires.
    let dir = TempDir::new().unwrap();
    let pad = "p".repeat(220);
    for i in 0..500 {
        let name = format!("file_{i:05}_{pad}.txt");
        tokio::fs::write(dir.path().join(&name), "x").await.unwrap();
    }

    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap()}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    assert_eq!(
        v["truncated"].as_bool().unwrap(),
        true,
        "truncated must be true when byte-budget cap fires"
    );
    let fallback = v["text_fallback"].as_str().unwrap();
    assert!(
        fallback.ends_with(" more results truncated (output capped at ~100 KB)"),
        "text_fallback must end with byte-cap trailer: ...{:?}",
        &fallback[fallback.len().saturating_sub(120)..]
    );
}

// ── Offset pagination tests ─────────────────────────────────────────────────

#[tokio::test]
async fn offset_skips_first_n_sorted_entries() {
    // Three files with distinct mtimes. offset=1 must skip the newest and
    // return only the middle and oldest entries (in mtime order).
    let dir = TempDir::new().unwrap();
    let newest = dir.path().join("newest.rs");
    let middle = dir.path().join("middle.rs");
    let oldest = dir.path().join("oldest.rs");
    tokio::fs::write(&newest, "n").await.unwrap();
    tokio::fs::write(&middle, "m").await.unwrap();
    tokio::fs::write(&oldest, "o").await.unwrap();

    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&oldest, t0);
    set_mtime(&middle, t0 + Duration::from_secs(60));
    set_mtime(&newest, t0 + Duration::from_secs(120));

    let out = Glob
        .invoke(
            json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "offset": 1}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    let matches = v["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2, "offset=1 must skip the newest entry");
    assert!(
        matches[0]["path"].as_str().unwrap().contains("middle.rs"),
        "first result after skip must be middle.rs (second newest)"
    );
    assert!(
        matches[1]["path"].as_str().unwrap().contains("oldest.rs"),
        "second result after skip must be oldest.rs"
    );
    assert_eq!(v["truncated"].as_bool().unwrap(), false);
}

#[tokio::test]
async fn offset_oversized_returns_empty_with_truncated_false() {
    // offset >= total matches must return matches: [], truncated: false — not
    // an error. We have 3 files and use offset=5.
    let dir = TempDir::new().unwrap();
    for name in ["a.rs", "b.rs", "c.rs"] {
        tokio::fs::write(dir.path().join(name), "x").await.unwrap();
    }

    let out = Glob
        .invoke(
            json!({"pattern": "*.rs", "path": dir.path().to_str().unwrap(), "offset": 5}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    assert!(
        v["matches"].as_array().unwrap().is_empty(),
        "oversized offset must yield empty matches array"
    );
    assert_eq!(
        v["truncated"].as_bool().unwrap(),
        false,
        "oversized offset must not flip truncated"
    );
    assert_eq!(
        v["text_fallback"].as_str().unwrap(),
        NO_RESULTS_MESSAGE,
        "text_fallback must equal NO_RESULTS_MESSAGE when offset exhausts matches"
    );
}

#[tokio::test]
async fn truncated_still_flips_after_offset_on_byte_budget() {
    // Even after skipping a few entries via offset, if the remaining entries
    // exceed the byte budget, truncated must still be true.
    let dir = TempDir::new().unwrap();
    let pad = "p".repeat(220);
    let total = 500usize;
    for i in 0..total {
        let name = format!("file_{i:05}_{pad}.txt");
        tokio::fs::write(dir.path().join(&name), "x").await.unwrap();
    }

    // Skip a few entries but leave plenty remaining to hit the byte budget.
    let out = Glob
        .invoke(
            json!({"pattern": "*.txt", "path": dir.path().to_str().unwrap(), "offset": 5}),
            &ctx(),
        )
        .await
        .unwrap();
    let v = structured_of(out);

    assert_eq!(
        v["truncated"].as_bool().unwrap(),
        true,
        "truncated must be true when byte-budget fires after offset skip"
    );
    let fallback = v["text_fallback"].as_str().unwrap();
    assert!(
        fallback.ends_with(" more results truncated (output capped at ~100 KB)"),
        "text_fallback must include byte-cap trailer: ...{:?}",
        &fallback[fallback.len().saturating_sub(120)..]
    );
}
