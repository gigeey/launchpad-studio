//! Unit tests for the Grep tool.
//!
//! Declared from `mod.rs` as `#[cfg(test)] mod tests;` 
//! per-tool folder layout — `tests.rs` is the same module as `mod.rs`,
//! so private items (constants, helpers) are in scope here.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{IoTool, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use jsonschema::Validator;
use serde_json::json;
use tempfile::TempDir;

use super::{
    Grep, CANCELLED_MESSAGE, MAX_LINE_CHARS, MAX_RESULT_BYTES, NO_FILES_MESSAGE, NO_MATCHES_MESSAGE,
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

fn unwrap_text(out: ToolOutput) -> String {
    match out {
        ToolOutput::Text(s) => s,
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Build a small fixture tree for content/files/count tests. Returns the
/// tempdir (kept alive by caller) and a list of (relative_path, content)
/// entries written.
async fn build_fixture(dir: &Path) {
    tokio::fs::write(dir.join("a.txt"), "alpha\nfoo bar\nbeta\n")
        .await
        .unwrap();
    tokio::fs::write(dir.join("b.txt"), "Foo Bar\nfoo again\n")
        .await
        .unwrap();
    tokio::fs::create_dir(dir.join("sub")).await.unwrap();
    tokio::fs::write(dir.join("sub").join("c.rs"), "fn foo() {}\nfn bar() {}\n")
        .await
        .unwrap();
}

#[test]
fn name_and_concurrency_safe() {
    let g = Grep;
    assert_eq!(g.name(), "Grep");
    assert!(g.is_concurrency_safe());
}

#[test]
fn description_returns_prompt_constant() {
    let g = Grep;
    assert_eq!(g.description(), super::prompt::DESCRIPTION);
    assert!(!g.description().is_empty());
    assert!(g
        .description()
        .starts_with("A powerful search tool built on ripgrep"));
}

#[test]
fn input_schema_is_self_contained_and_valid() {
    let schema = Grep.input_schema();
    let validator = Validator::new(&schema).expect("schema must compile");

    let pattern_only = json!({"pattern": "foo"});
    assert!(validator.is_valid(&pattern_only));

    let kitchen_sink = json!({
        "pattern": "foo",
        "path": "/tmp",
        "glob": "*.rs",
        "type": "rust",
        "output_mode": "content",
        "-A": 1,
        "-B": 2,
        "-C": 3,
        "context": 1,
        "-i": true,
        "-n": false,
        "multiline": true,
        "head_limit": 50,
        "offset": 5,
    });
    assert!(validator.is_valid(&kitchen_sink));

    let missing_required = json!({"path": "/tmp"});
    assert!(!validator.is_valid(&missing_required));

    let bad_mode = json!({"pattern": "foo", "output_mode": "weird"});
    assert!(!validator.is_valid(&bad_mode));

    let unknown_field = json!({"pattern": "foo", "extra": 1});
    assert!(!validator.is_valid(&unknown_field));
}

#[test]
fn implementation_does_not_shell_out() {
    // Acceptance criterion: "no Command::new(\"rg\") or std::process::Command
    // anywhere in src/grep/". Read the implementation files and assert.
    // tests.rs is excluded because the assertion strings here would be
    // false positives.
    for (name, src) in [
        ("mod.rs", include_str!("mod.rs")),
        ("prompt.rs", include_str!("prompt.rs")),
    ] {
        assert!(
            !src.contains("std::process::Command"),
            "{name} must not shell out via std::process::Command"
        );
        assert!(
            !src.contains("Command::new(\"rg\")"),
            "{name} must not shell out to the rg binary"
        );
    }
}

#[tokio::test]
async fn files_with_matches_returns_absolute_paths_newline_joined() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);

    let lines: Vec<&str> = text.lines().collect();
    // First line is the "Found N files" summary; remaining lines are paths.
    assert!(
        lines[0].starts_with("Found "),
        "expected summary first line, got {text:?}"
    );
    assert!(
        lines[1..].iter().all(|l| l.starts_with('/')),
        "got {text:?}"
    );
    assert!(text.contains("a.txt"));
    assert!(text.contains("b.txt"));
    assert!(text.contains("c.rs"));
}

#[tokio::test]
async fn content_mode_returns_path_lineno_line() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                // Filter to a single file so the assertions are cheap.
                "glob": "a.txt",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);

    // Expected match line: <abs_path>:2:foo bar
    let path = dir.path().join("a.txt");
    let expected = format!("{}:2:foo bar", path.to_string_lossy());
    assert!(
        text.contains(&expected),
        "content output {text:?} missing {expected:?}"
    );
}

#[tokio::test]
async fn content_mode_with_context_separates_blocks_with_dashes() {
    let dir = TempDir::new().unwrap();
    // Two non-contiguous matches, with two non-matching lines between
    // them so a -C 1 window leaves a gap → `--` between blocks.
    let p = dir.path().join("ctx.txt");
    tokio::fs::write(
        &p,
        "context_above_1\nMATCH_ONE\ncontext_below_1\nfiller\nfiller2\ncontext_above_2\nMATCH_TWO\ncontext_below_2\n",
    )
    .await
    .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "MATCH_(ONE|TWO)",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "-C": 1,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);

    assert!(
        text.contains("\n--\n"),
        "expected `--` separator between blocks, got {text:?}"
    );
    // Both matches present.
    assert!(text.contains("MATCH_ONE"));
    assert!(text.contains("MATCH_TWO"));
    // Both context lines present.
    assert!(text.contains("context_above_1"));
    assert!(text.contains("context_below_2"));
}

#[tokio::test]
async fn count_mode_returns_path_n_sorted_desc() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("one.txt"), "foo\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("three.txt"), "foo\nfoo\nfoo\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("two.txt"), "foo\nfoo\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);

    let lines: Vec<&str> = text.lines().collect();
    // 3 per-file lines + 1 summary line.
    assert_eq!(lines.len(), 4, "got {text:?}");
    // Highest count first.
    assert!(lines[0].ends_with(":3"), "first line {:?} not :3", lines[0]);
    assert!(lines[1].ends_with(":2"));
    assert!(lines[2].ends_with(":1"));
    // Summary line: N = 3+2+1 = 6 total occurrences across 3 files.
    assert_eq!(lines[3], "Found 6 total occurrences across 3 files");
}

#[tokio::test]
async fn case_insensitive_flag_honored() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "FooBarBaz\n")
        .await
        .unwrap();

    // Without -i: no match (files_with_matches mode → "No files found").
    let out = Grep
        .invoke(
            json!({
                "pattern": "foobar",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(unwrap_text(out), NO_FILES_MESSAGE);

    // With -i: matches.
    let out = Grep
        .invoke(
            json!({
                "pattern": "foobar",
                "path": dir.path().to_str().unwrap(),
                "-i": true,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("a.txt"), "got {text:?}");
}

#[tokio::test]
async fn glob_filter_restricts_files() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "glob": "*.txt",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);

    assert!(text.contains("a.txt"));
    assert!(text.contains("b.txt"));
    assert!(
        !text.contains("c.rs"),
        "glob *.txt must exclude .rs: {text:?}"
    );
}

#[tokio::test]
async fn type_filter_restricts_files() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "type": "rust",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);

    assert!(text.contains("c.rs"));
    assert!(!text.contains("a.txt"));
    assert!(!text.contains("b.txt"));
}

#[tokio::test]
async fn multiline_matches_across_newlines() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("ml.txt"), "begin\nmiddle\nend\n")
        .await
        .unwrap();

    // Without multiline=true the regex engine is line-anchored and
    // begin.*end never matches (files_with_matches mode → "No files found").
    let out = Grep
        .invoke(
            json!({
                "pattern": "begin[\\s\\S]*end",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(unwrap_text(out), NO_FILES_MESSAGE);

    // With multiline=true the same pattern spans the file.
    let out = Grep
        .invoke(
            json!({
                "pattern": "begin[\\s\\S]*end",
                "path": dir.path().to_str().unwrap(),
                "multiline": true,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("ml.txt"), "got {text:?}");
}

#[tokio::test]
async fn respects_gitignore_by_default() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".gitignore"), "secret.txt\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("kept.txt"), "foo\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("secret.txt"), "foo\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);

    assert!(text.contains("kept.txt"));
    assert!(
        !text.contains("secret.txt"),
        "gitignored file leaked: {text:?}"
    );
}

#[tokio::test]
async fn regex_compile_error_returns_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let out = Grep
        .invoke(
            json!({
                // Unclosed character class is a syntax error.
                "pattern": "[abc",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "invalid regex pattern");
}

// UNC-path guard (Windows only).

#[cfg(windows)]
#[tokio::test]
async fn unc_path_returns_recoverable_error() {
    // UNC paths (\\server\share) must be rejected before any filesystem I/O
    // to prevent NTLM credential probing via model-supplied paths.
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": r"\\server\share",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "UNC paths are not supported");
}

// Path expansion — tilde, relative, absolute, omitted (cwd default).

#[tokio::test]
async fn tilde_path_resolves_not_absolute_path_error() {
    // A tilde-prefixed path that does not exist on disk should produce
    // "path does not exist" — NOT the old "absolute path" error that fired
    // before tilde expansion was wired in.
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": "~/launchpad_grep_us001_nonexistent_xyz",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "path does not exist");
}

#[tokio::test]
async fn relative_path_resolves_not_absolute_path_error() {
    // A relative path that does not exist on disk should produce
    // "path does not exist" — NOT the old "absolute path" error that fired
    // before path expansion was wired in.
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": "launchpad_grep_us001_nonexistent_relative",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "path does not exist");
}

#[tokio::test]
async fn absolute_path_still_works() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("abs.txt"), "foo\n")
        .await
        .unwrap();
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("abs.txt"), "got {text:?}");
}

#[tokio::test]
async fn missing_path_defaults_to_cwd() {
    // Omitting `path` should default to cwd, which always exists, so the
    // tool runs without a stat error (result content is unimportant here).
    let out = Grep
        .invoke(
            json!({ "pattern": "launchpad_grep_us001_unlikely_to_match_xyzzy" }),
            &ctx(),
        )
        .await
        .unwrap();
    // Either no matches or actual matches — but never a stat/path error.
    match out {
        ToolOutput::Text(_) => {}
        ToolOutput::Error { message, .. } => {
            panic!("expected successful (possibly empty) search, got error: {message}")
        }
        _ => {}
    }
}

// Output paths relativized under cwd.

#[tokio::test]
async fn hit_under_cwd_renders_relative_in_files_with_matches() {
    // Create a tempdir inside cwd so hits fall under cwd and render relative.
    let cwd = std::env::current_dir().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("grep_us003_fwm_")
        .tempdir_in(&cwd)
        .unwrap();
    tokio::fs::write(dir.path().join("rel.txt"), "us003_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us003_marker",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 1 summary line + 1 path line.
    assert_eq!(lines.len(), 2, "got {text:?}");
    assert_eq!(
        lines[0], "Found 1 file",
        "expected summary as first line, got {text:?}"
    );
    // Path is relative (does not start with '/').
    assert!(
        !lines[1].starts_with('/'),
        "expected relative path, got {:?}",
        lines[1]
    );
    assert!(lines[1].ends_with("rel.txt"), "got {:?}", lines[1]);
}

#[tokio::test]
async fn hit_under_cwd_renders_relative_in_count_mode() {
    let cwd = std::env::current_dir().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("grep_us003_cnt_")
        .tempdir_in(&cwd)
        .unwrap();
    tokio::fs::write(dir.path().join("cnt.txt"), "us003_marker\nus003_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us003_marker",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 1 per-file line + 1 summary line.
    assert_eq!(lines.len(), 2, "got {text:?}");
    // Format is `<relative_path>:<count>` — path should not start with '/'.
    let (path_part, count_part) = lines[0].rsplit_once(':').unwrap();
    assert_eq!(count_part, "2");
    assert!(
        !path_part.starts_with('/'),
        "expected relative path, got {:?}",
        path_part
    );
    assert!(path_part.ends_with("cnt.txt"), "got {:?}", path_part);
    assert_eq!(lines[1], "Found 2 total occurrences across 1 file");
}

#[tokio::test]
async fn hit_under_cwd_renders_relative_in_content_mode() {
    let cwd = std::env::current_dir().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("grep_us003_con_")
        .tempdir_in(&cwd)
        .unwrap();
    tokio::fs::write(dir.path().join("con.txt"), "us003_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us003_marker",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    // Format: `<path>:1:us003_marker`
    let first_colon = text.find(':').unwrap();
    let path_part = &text[..first_colon];
    assert!(
        !path_part.starts_with('/'),
        "expected relative path in content output, got {:?}",
        text
    );
    assert!(path_part.ends_with("con.txt"), "got {:?}", text);
}

#[tokio::test]
async fn hit_outside_cwd_renders_absolute() {
    // TempDir::new() creates a dir in the system temp dir, which is outside
    // the runner cwd — hits must fall back to absolute paths.
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("abs.txt"), "us003_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us003_marker",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 1 summary line + 1 path line.
    assert_eq!(lines.len(), 2, "got {text:?}");
    assert_eq!(
        lines[0], "Found 1 file",
        "expected summary as first line, got {text:?}"
    );
    // On POSIX the absolute path starts with '/'.
    assert!(
        lines[1].starts_with('/'),
        "expected absolute path for hit outside cwd, got {:?}",
        lines[1]
    );
}

#[tokio::test]
async fn context_flag_with_count_mode_is_recoverable_error() {
    let dir = TempDir::new().unwrap();
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
                "-A": 1,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "content");
}

#[tokio::test]
async fn offset_and_head_limit_apply_in_correct_order() {
    let dir = TempDir::new().unwrap();
    // Build 5 files each with one match. Output mode = files_with_matches
    // sorts by mtime desc; we set mtimes in known order so we can predict
    // which paths appear after offset+limit.
    use std::fs::{File, FileTimes};
    use std::time::SystemTime;
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut paths = Vec::new();
    for i in 0..5 {
        let p = dir.path().join(format!("f_{i}.txt"));
        tokio::fs::write(&p, "foo\n").await.unwrap();
        let f = File::options().write(true).open(&p).unwrap();
        // Higher index → newer mtime, so f_4 sorts first.
        f.set_times(FileTimes::new().set_modified(t0 + Duration::from_secs(60 * i as u64)))
            .unwrap();
        paths.push(p);
    }

    // offset=1, head_limit=2 against [f_4, f_3, f_2, f_1, f_0] should
    // yield [f_3, f_2] with a pagination footer.
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "offset": 1,
                "head_limit": 2,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 1 summary line + 2 path lines + 1 pagination footer.
    assert_eq!(lines.len(), 4, "got {text:?}");
    assert_eq!(
        lines[0], "Found 2 files",
        "expected summary first, got {text:?}"
    );
    assert!(lines[1].ends_with("f_3.txt"), "first {:?}", lines[1]);
    assert!(lines[2].ends_with("f_2.txt"), "second {:?}", lines[2]);
    assert_eq!(
        lines[3], "[paginated: limit=2 offset=1]",
        "footer mismatch; got {text:?}"
    );
}

#[tokio::test]
async fn cancellation_returns_cancelled_within_100ms() {
    let dir = TempDir::new().unwrap();
    for i in 0..10 {
        tokio::fs::write(dir.path().join(format!("f_{i}.txt")), "foo\n")
            .await
            .unwrap();
    }

    let context = ctx();
    context.cancel.cancel();

    let path_s = dir.path().to_str().unwrap().to_string();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        Grep.invoke(
            json!({
                "pattern": "foo",
                "path": path_s,
            }),
            &context,
        ),
    )
    .await
    .expect("must return within 100ms");

    match result {
        Err(AoError::Internal(msg)) => assert_eq!(msg, CANCELLED_MESSAGE),
        other => panic!("expected AoError::Internal(\"cancelled\"), got {other:?}"),
    }
}

#[tokio::test]
async fn dispatches_via_registry() {
    use ao_engine_tools_core::Registry;

    let dir = TempDir::new().unwrap();
    let p = dir.path().join("hi.txt");
    tokio::fs::write(&p, "foo\n").await.unwrap();

    let mut r = Registry::new();
    r.register_io(Arc::new(Grep));
    let context = RunnerContext::new("s", "a")
        .unwrap()
        .with_registry(Arc::new(r));

    let tool = context.registry.lookup_io("Grep").expect("registered");
    let out = tool
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
            }),
            &context,
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains(p.to_str().unwrap()), "got {text:?}");
}

// Total-result byte cap (~100 KB).

#[test]
fn byte_cap_under_limit_returns_unchanged() {
    let body = "line1\nline2\nline3\n".to_string();
    assert_eq!(super::truncate_to_byte_cap(body.clone()), body);
}

#[test]
fn byte_cap_over_limit_truncates_at_line_boundary_with_marker() {
    // Each line is 99 'a's + '\n' = 100 bytes.
    let line = "a".repeat(99) + "\n";
    let count = MAX_RESULT_BYTES / 100 + 5;
    let body: String = line.repeat(count);
    assert!(body.len() > MAX_RESULT_BYTES);

    let result = super::truncate_to_byte_cap(body);

    let lines: Vec<&str> = result.lines().collect();
    // [truncated] is the last line.
    assert_eq!(*lines.last().unwrap(), "[truncated]");
    // Second-to-last line is a complete content line.
    assert_eq!(lines[lines.len() - 2], "a".repeat(99));
    // Result does not exceed the cap plus the marker and one newline.
    assert!(
        result.len() <= MAX_RESULT_BYTES + "[truncated]".len() + 1,
        "result len {}",
        result.len()
    );
}

#[tokio::test]
async fn byte_cap_content_mode_via_invoke() {
    let dir = TempDir::new().unwrap();
    // 700 lines × (128 + ~30) chars of output ≈ 112 KB > MAX_RESULT_BYTES.
    let content: String = (0..700)
        .map(|_| format!("us004_cap_marker {}\n", "x".repeat(128)))
        .collect();
    tokio::fs::write(dir.path().join("big.txt"), &content)
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us004_cap_marker",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "head_limit": 0,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.ends_with("[truncated]"),
        "expected truncated output; text len {} ending: ...{}",
        text.len(),
        &text[text.len().saturating_sub(50)..]
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(*lines.last().unwrap(), "[truncated]");
}

#[tokio::test]
async fn byte_cap_files_with_matches_mode_via_invoke() {
    let dir = TempDir::new().unwrap();
    // A 200-char subdir name ensures each absolute path exceeds 210 chars
    // regardless of the system temp-dir prefix length.
    // 500 files × 211+ chars ≈ 105 KB > MAX_RESULT_BYTES.
    let long_subdir = "x".repeat(200);
    let subdir = dir.path().join(&long_subdir);
    std::fs::create_dir_all(&subdir).unwrap();
    {
        let base = subdir.clone();
        tokio::task::spawn_blocking(move || {
            for i in 0..500u32 {
                std::fs::write(base.join(format!("f{:04}.txt", i)), b"us004_fwm\n").unwrap();
            }
        })
        .await
        .unwrap();
    }

    let out = Grep
        .invoke(
            json!({
                "pattern": "us004_fwm",
                "path": subdir.to_str().unwrap(),
                "head_limit": 0,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.ends_with("[truncated]"),
        "expected truncated output; text len {} ending: ...{}",
        text.len(),
        &text[text.len().saturating_sub(50)..]
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(*lines.last().unwrap(), "[truncated]");
}

#[tokio::test]
async fn byte_cap_count_mode_via_invoke() {
    let dir = TempDir::new().unwrap();
    // Same 200-char subdir trick as files_with_matches test. Count output is
    // `<path>:1` per file — same long-path budget.
    let long_subdir = "x".repeat(200);
    let subdir = dir.path().join(&long_subdir);
    std::fs::create_dir_all(&subdir).unwrap();
    {
        let base = subdir.clone();
        tokio::task::spawn_blocking(move || {
            for i in 0..500u32 {
                std::fs::write(base.join(format!("f{:04}.txt", i)), b"us004_cnt\n").unwrap();
            }
        })
        .await
        .unwrap();
    }

    let out = Grep
        .invoke(
            json!({
                "pattern": "us004_cnt",
                "path": subdir.to_str().unwrap(),
                "output_mode": "count",
                "head_limit": 0,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.ends_with("[truncated]"),
        "expected truncated output; text len {} ending: ...{}",
        text.len(),
        &text[text.len().saturating_sub(50)..]
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(*lines.last().unwrap(), "[truncated]");
}

// Mode-specific no-matches messages.

#[tokio::test]
async fn no_match_files_with_matches_mode_emits_no_files_found() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "hello\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us005_nomatch_xyz",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "files_with_matches",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(unwrap_text(out), NO_FILES_MESSAGE);
}

#[tokio::test]
async fn no_match_content_mode_emits_no_matches_found() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "hello\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us005_nomatch_xyz",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(unwrap_text(out), NO_MATCHES_MESSAGE);
}

#[tokio::test]
async fn no_match_count_mode_emits_no_matches_found() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "hello\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us005_nomatch_xyz",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_eq!(unwrap_text(out), NO_MATCHES_MESSAGE);
}

// Count-mode summary line.

#[tokio::test]
async fn count_mode_empty_result_has_no_summary_line() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "hello\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us006_nomatch_xyz",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    // Empty result: only the no-matches message, no summary line.
    let text = unwrap_text(out);
    assert_eq!(text, NO_MATCHES_MESSAGE);
    assert!(
        !text.contains("Found"),
        "unexpected summary in empty result: {text:?}"
    );
}

#[tokio::test]
async fn count_mode_summary_line_correct_n_and_m() {
    let dir = TempDir::new().unwrap();
    // 2 files: one with 3 matches, one with 1 match → N=4, M=2.
    tokio::fs::write(dir.path().join("many.txt"), "us006\nus006\nus006\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("one.txt"), "us006\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us006",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 2 per-file lines + 1 summary.
    assert_eq!(lines.len(), 3, "got {text:?}");
    assert_eq!(lines[2], "Found 4 total occurrences across 2 files");
}

#[tokio::test]
async fn count_mode_summary_sums_across_multiple_files() {
    let dir = TempDir::new().unwrap();
    // 4 files with varying counts: 5, 3, 3, 1 → N=12, M=4.
    tokio::fs::write(
        dir.path().join("a.txt"),
        "us006s\nus006s\nus006s\nus006s\nus006s\n",
    )
    .await
    .unwrap();
    tokio::fs::write(dir.path().join("b.txt"), "us006s\nus006s\nus006s\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("c.txt"), "us006s\nus006s\nus006s\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("d.txt"), "us006s\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us006s",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 4 per-file lines + 1 summary.
    assert_eq!(lines.len(), 5, "got {text:?}");
    assert_eq!(
        lines[4], "Found 12 total occurrences across 4 files",
        "summary mismatch; full output: {text:?}"
    );
}

// files_with_matches summary line.

#[tokio::test]
async fn files_with_matches_summary_single_file_is_first_line() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("only.txt"), "us007_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us007_marker",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // Summary line first, then 1 file path.
    assert_eq!(lines.len(), 2, "got {text:?}");
    assert_eq!(
        lines[0], "Found 1 file",
        "summary first line mismatch; got {text:?}"
    );
    assert!(lines[1].ends_with("only.txt"), "got {text:?}");
}

#[tokio::test]
async fn files_with_matches_summary_multi_file_correct_n() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("x.txt"), "us007_multi\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("y.txt"), "us007_multi\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("z.txt"), "us007_multi\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us007_multi",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // Summary line first, then 3 file paths.
    assert_eq!(lines.len(), 4, "got {text:?}");
    assert_eq!(
        lines[0], "Found 3 files",
        "summary first line mismatch; got {text:?}"
    );
}

#[tokio::test]
async fn files_with_matches_empty_has_no_summary() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "hello\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us007_nomatch_xyz",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert_eq!(
        text, NO_FILES_MESSAGE,
        "empty result must not have summary: {text:?}"
    );
    assert!(
        !text.contains("Found"),
        "unexpected summary in empty result: {text:?}"
    );
}

// Count-mode busiest-first sort order (gap #12).

#[tokio::test]
async fn count_mode_busiest_first_with_lex_tiebreak() {
    // 4 files: aa.txt (5 hits), ab.txt (3 hits), ba.txt (3 hits), c.txt (1 hit).
    // Expected order: aa.txt:5, ab.txt:3, ba.txt:3, c.txt:1 (busiest first;
    // ab < ba lexicographically for the tied-at-3 pair).
    let dir = TempDir::new().unwrap();
    tokio::fs::write(
        dir.path().join("aa.txt"),
        "us009\nus009\nus009\nus009\nus009\n",
    )
    .await
    .unwrap();
    tokio::fs::write(dir.path().join("ab.txt"), "us009\nus009\nus009\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("ba.txt"), "us009\nus009\nus009\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("c.txt"), "us009\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us009",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 4 per-file lines + 1 summary line.
    assert_eq!(lines.len(), 5, "got {text:?}");
    // Busiest first.
    assert!(
        lines[0].ends_with(":5"),
        "first line should be :5, got {:?}",
        lines[0]
    );
    // Equal-count pair (3) — ab.txt < ba.txt lexicographically.
    assert!(
        lines[1].ends_with(":3") && lines[1].contains("ab.txt"),
        "second line should be ab.txt:3, got {:?}",
        lines[1]
    );
    assert!(
        lines[2].ends_with(":3") && lines[2].contains("ba.txt"),
        "third line should be ba.txt:3, got {:?}",
        lines[2]
    );
    // Least matches last.
    assert!(
        lines[3].ends_with(":1"),
        "fourth line should be :1, got {:?}",
        lines[3]
    );
    // Summary: 5+3+3+1 = 12 total across 4 files.
    assert_eq!(
        lines[4], "Found 12 total occurrences across 4 files",
        "got {text:?}"
    );
}

// (batch 3): pagination footer + opt-in `offset` field.

#[tokio::test]
async fn pagination_footer_fires_on_head_limit_truncation_files_mode() {
    let dir = TempDir::new().unwrap();
    for name in &["a.txt", "b.txt", "c.txt"] {
        tokio::fs::write(dir.path().join(name), "us1_paginate\n")
            .await
            .unwrap();
    }

    // head_limit=1 with 3 matches → truncated → footer fires.
    let out = Grep
        .invoke(
            json!({
                "pattern": "us1_paginate",
                "path": dir.path().to_str().unwrap(),
                "head_limit": 1,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // summary + 1 path + footer
    assert_eq!(lines.len(), 3, "got {text:?}");
    assert_eq!(
        lines[2], "[paginated: limit=1 offset=0]",
        "footer mismatch; got {text:?}"
    );
}

#[tokio::test]
async fn pagination_footer_fires_on_nonzero_offset_files_mode() {
    let dir = TempDir::new().unwrap();
    for name in &["a.txt", "b.txt"] {
        tokio::fs::write(dir.path().join(name), "us1_offset\n")
            .await
            .unwrap();
    }

    // offset=1, head_limit=250 (default) → offset > 0 → footer fires even though no truncation.
    let out = Grep
        .invoke(
            json!({
                "pattern": "us1_offset",
                "path": dir.path().to_str().unwrap(),
                "offset": 1,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // summary + 1 path + footer
    assert_eq!(lines.len(), 3, "got {text:?}");
    assert_eq!(
        lines[2], "[paginated: limit=250 offset=1]",
        "footer mismatch; got {text:?}"
    );
}

#[tokio::test]
async fn pagination_footer_both_truncation_and_offset_correct_values() {
    let dir = TempDir::new().unwrap();
    for name in &["a.txt", "b.txt", "c.txt", "d.txt"] {
        tokio::fs::write(dir.path().join(name), "us1_both\n")
            .await
            .unwrap();
    }

    // 4 files, offset=1 → 3 remain; head_limit=1 → truncated to 1.
    let out = Grep
        .invoke(
            json!({
                "pattern": "us1_both",
                "path": dir.path().to_str().unwrap(),
                "offset": 1,
                "head_limit": 1,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // summary + 1 path + footer
    assert_eq!(lines.len(), 3, "got {text:?}");
    assert_eq!(
        lines[2], "[paginated: limit=1 offset=1]",
        "footer mismatch; got {text:?}"
    );
}

#[tokio::test]
async fn no_pagination_footer_when_under_cap_and_no_offset() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("x.txt"), "us1_nofooter\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us1_nofooter",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        !text.contains("[paginated:"),
        "no footer expected when no truncation and no offset; got {text:?}"
    );
}

#[tokio::test]
async fn pagination_footer_fires_in_count_mode() {
    let dir = TempDir::new().unwrap();
    for name in &["a.txt", "b.txt", "c.txt"] {
        tokio::fs::write(dir.path().join(name), "us1_cnt\n")
            .await
            .unwrap();
    }

    // head_limit=1 with 3 matching files → truncated.
    let out = Grep
        .invoke(
            json!({
                "pattern": "us1_cnt",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "count",
                "head_limit": 1,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 1 per-file line + summary + footer
    assert_eq!(lines.len(), 3, "got {text:?}");
    assert_eq!(
        lines[2], "[paginated: limit=1 offset=0]",
        "footer mismatch; got {text:?}"
    );
}

#[tokio::test]
async fn pagination_footer_fires_in_content_mode() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("m.txt"), "us1_con\nus1_con\nus1_con\n")
        .await
        .unwrap();

    // 3 match lines, head_limit=2 → truncated.
    let out = Grep
        .invoke(
            json!({
                "pattern": "us1_con",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "head_limit": 2,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // 2 content lines + footer
    assert_eq!(lines.len(), 3, "got {text:?}");
    assert_eq!(
        lines[2], "[paginated: limit=2 offset=0]",
        "footer mismatch; got {text:?}"
    );
}

#[tokio::test]
async fn offset_past_total_yields_empty_body() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("only.txt"), "us1_empty\n")
        .await
        .unwrap();

    // 1 match, offset=5 → no entries → empty message, no footer.
    let out = Grep
        .invoke(
            json!({
                "pattern": "us1_empty",
                "path": dir.path().to_str().unwrap(),
                "offset": 5,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert_eq!(text, NO_FILES_MESSAGE, "got {text:?}");
    assert!(
        !text.contains("[paginated:"),
        "no footer on empty result; got {text:?}"
    );
}

// (batch 3): long-line cap with `[long line N chars]` marker (gap #8).

#[tokio::test]
async fn content_mode_short_line_emitted_unchanged() {
    let dir = TempDir::new().unwrap();
    let short_line = "x".repeat(10);
    tokio::fs::write(dir.path().join("short.txt"), format!("{}\n", short_line))
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "x+",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.contains(&short_line),
        "short line must be emitted unchanged; got {text:?}"
    );
    assert!(
        !text.contains("[long line"),
        "no long-line marker expected for short line; got {text:?}"
    );
}

#[tokio::test]
async fn content_mode_line_exactly_max_chars_emitted_unchanged() {
    let dir = TempDir::new().unwrap();
    let exact_line = "y".repeat(MAX_LINE_CHARS);
    tokio::fs::write(dir.path().join("exact.txt"), format!("{}\n", exact_line))
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "y+",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "head_limit": 0,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.contains(&exact_line),
        "line of exactly {} chars must be emitted unchanged; got text len {}",
        MAX_LINE_CHARS,
        text.len()
    );
    assert!(
        !text.contains("[long line"),
        "no long-line marker expected for line at the limit; got {text:?}"
    );
}

#[tokio::test]
async fn content_mode_line_over_max_chars_replaced_with_marker() {
    let dir = TempDir::new().unwrap();
    let long_line = "z".repeat(MAX_LINE_CHARS + 1);
    tokio::fs::write(dir.path().join("long.txt"), format!("{}\n", long_line))
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "z+",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "head_limit": 0,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let expected_marker = format!("[long line {} chars]", MAX_LINE_CHARS + 1);
    assert!(
        text.contains(&expected_marker),
        "expected marker {expected_marker:?} for line over limit; got {text:?}"
    );
    assert!(
        !text.contains(&long_line),
        "full long line content must not appear in output; got text len {}",
        text.len()
    );
}

#[tokio::test]
async fn content_mode_long_line_path_prefix_preserved() {
    let dir = TempDir::new().unwrap();
    let long_line = "w".repeat(MAX_LINE_CHARS + 50);
    tokio::fs::write(dir.path().join("prefix.txt"), format!("{}\n", long_line))
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "w+",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "head_limit": 0,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    // Format must be `<path>:<line_no>:[long line N chars]`
    assert!(
        text.contains("prefix.txt:1:[long line"),
        "path:line_no: prefix must be preserved; got {text:?}"
    );
}

#[tokio::test]
async fn content_mode_multibyte_chars_counted_as_chars_not_bytes() {
    let dir = TempDir::new().unwrap();
    // Each '€' is 3 bytes but 1 char. 501 '€' chars → 1503 bytes but 501 chars → should trigger marker.
    // 500 '€' chars → 1500 bytes but 500 chars → should NOT trigger marker.
    let at_limit = "€".repeat(MAX_LINE_CHARS);
    let over_limit = "€".repeat(MAX_LINE_CHARS + 1);
    tokio::fs::write(
        dir.path().join("multibyte.txt"),
        format!("{}\n{}\n", at_limit, over_limit),
    )
    .await
    .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "€+",
                "path": dir.path().to_str().unwrap(),
                "output_mode": "content",
                "head_limit": 0,
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    // The 501-char line must be replaced with a marker.
    let expected_marker = format!("[long line {} chars]", MAX_LINE_CHARS + 1);
    assert!(
        text.contains(&expected_marker),
        "501-char multibyte line must produce marker {expected_marker:?}; got text len {}",
        text.len()
    );
    // The 500-char line must be emitted as-is (contains '€' sequences, not a marker).
    // We can't easily assert the full at_limit string in a possibly-truncated output,
    // but we can assert that the first match line (lineno 1) does NOT show a long-line marker.
    assert!(
        !text.contains("[long line 500 chars]"),
        "500-char multibyte line must NOT produce a marker; got {text:?}"
    );
}

// Hidden ON + VCS-metadata deny-list (gaps #6 + #7 paired).

#[tokio::test]
async fn hidden_dotfile_is_matched() {
    // A hidden file like .env.example must be searchable with hidden=false.
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join(".env.example"), "us008_hidden_marker\n")
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("regular.txt"), "other\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us008_hidden_marker",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.contains(".env.example"),
        ".env.example must be searchable: {text:?}"
    );
}

#[tokio::test]
async fn git_objects_not_matched() {
    // Files under .git/objects must be excluded even when the pattern matches.
    let dir = TempDir::new().unwrap();
    let git_objects = dir.path().join(".git").join("objects");
    tokio::fs::create_dir_all(&git_objects).await.unwrap();
    tokio::fs::write(git_objects.join("deadbeef"), "us008_git_marker\n")
        .await
        .unwrap();
    // Regular file that should still be matched.
    tokio::fs::write(dir.path().join("normal.txt"), "us008_git_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us008_git_marker",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        !text.contains(".git"),
        ".git/objects must be excluded from results: {text:?}"
    );
    assert!(
        text.contains("normal.txt"),
        "regular file must still match: {text:?}"
    );
}

#[tokio::test]
async fn regular_file_matches_when_git_dir_present() {
    // A regular file in a project containing a .git directory must still match.
    let dir = TempDir::new().unwrap();
    tokio::fs::create_dir(dir.path().join(".git"))
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("src.txt"), "us008_regular_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us008_regular_marker",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.contains("src.txt"),
        "regular file must match even with .git dir present: {text:?}"
    );
}

// (batch 3): multi-glob support — split on whitespace + commas (gap #10).

#[test]
fn split_glob_single_pattern_returned_as_is() {
    let tokens = super::split_glob_patterns("*.txt");
    assert_eq!(tokens, vec!["*.txt"]);
}

#[test]
fn split_glob_whitespace_separated() {
    let tokens = super::split_glob_patterns("*.ts *.tsx");
    assert_eq!(tokens, vec!["*.ts", "*.tsx"]);
}

#[test]
fn split_glob_comma_separated() {
    let tokens = super::split_glob_patterns("*.ts,*.tsx");
    assert_eq!(tokens, vec!["*.ts", "*.tsx"]);
}

#[test]
fn split_glob_mixed_separators() {
    let tokens = super::split_glob_patterns("*.ts, *.tsx src/**/*.js");
    assert_eq!(tokens, vec!["*.ts", "*.tsx", "src/**/*.js"]);
}

#[test]
fn split_glob_brace_expression_preserved_as_single_token() {
    let tokens = super::split_glob_patterns("*.{ts,tsx}");
    assert_eq!(tokens, vec!["*.{ts,tsx}"]);
}

#[test]
fn split_glob_brace_expression_mixed_with_other_patterns() {
    let tokens = super::split_glob_patterns("*.{ts,tsx} src/**/*.js");
    assert_eq!(tokens, vec!["*.{ts,tsx}", "src/**/*.js"]);
}

#[test]
fn split_glob_empty_input_returns_empty() {
    let tokens = super::split_glob_patterns("");
    assert!(tokens.is_empty());
}

#[test]
fn split_glob_whitespace_only_returns_empty() {
    let tokens = super::split_glob_patterns("  ,  ");
    assert!(tokens.is_empty());
}

#[tokio::test]
async fn multi_glob_whitespace_separated_matches_both_extensions() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    // "*.txt *.rs" — should find a.txt, b.txt, and c.rs.
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "glob": "*.txt *.rs",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("a.txt"), "expected a.txt; got {text:?}");
    assert!(text.contains("b.txt"), "expected b.txt; got {text:?}");
    assert!(text.contains("c.rs"), "expected c.rs; got {text:?}");
}

#[tokio::test]
async fn multi_glob_comma_separated_matches_both_extensions() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "glob": "*.txt,*.rs",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("a.txt"), "expected a.txt; got {text:?}");
    assert!(text.contains("b.txt"), "expected b.txt; got {text:?}");
    assert!(text.contains("c.rs"), "expected c.rs; got {text:?}");
}

#[tokio::test]
async fn multi_glob_mixed_separators_matches_both_extensions() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "glob": "*.txt, *.rs",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("a.txt"), "expected a.txt; got {text:?}");
    assert!(text.contains("b.txt"), "expected b.txt; got {text:?}");
    assert!(text.contains("c.rs"), "expected c.rs; got {text:?}");
}

#[tokio::test]
async fn multi_glob_brace_expression_preserved_and_matches_both_extensions() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    // "*.{txt,rs}" is treated as a single token (not split on the inner comma).
    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "glob": "*.{txt,rs}",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("a.txt"), "expected a.txt; got {text:?}");
    assert!(text.contains("b.txt"), "expected b.txt; got {text:?}");
    assert!(text.contains("c.rs"), "expected c.rs; got {text:?}");
}

#[tokio::test]
async fn multi_glob_single_pattern_still_works() {
    let dir = TempDir::new().unwrap();
    build_fixture(dir.path()).await;

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                "glob": "*.txt",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("a.txt"), "expected a.txt; got {text:?}");
    assert!(text.contains("b.txt"), "expected b.txt; got {text:?}");
    assert!(
        !text.contains("c.rs"),
        "single *.txt glob must exclude .rs: {text:?}"
    );
}

#[tokio::test]
async fn multi_glob_invalid_token_recoverable_error_names_token() {
    let dir = TempDir::new().unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "foo",
                "path": dir.path().to_str().unwrap(),
                // "[invalid" is an unclosed character class — invalid as a glob
                "glob": "*.txt [invalid",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "[invalid");
}

// ENOENT "did you mean" suggestion tests

#[tokio::test]
async fn enoent_near_miss_yields_suggestion() {
    let dir = TempDir::new().unwrap();
    // Create a `src` subdirectory so the similarity helper can find it.
    tokio::fs::create_dir(dir.path().join("src")).await.unwrap();

    // Request `srd` — one edit away from `src`.
    let missing = dir.path().join("srd");
    let out = Grep
        .invoke(
            json!({
                "pattern": "anything",
                "path": missing.to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    assert_recoverable_error(out, "Did you mean src?");
}

#[tokio::test]
async fn enoent_no_near_miss_unchanged_error() {
    let dir = TempDir::new().unwrap();
    // Create a `src` subdirectory — not similar enough to `zzzzzzz`.
    tokio::fs::create_dir(dir.path().join("src")).await.unwrap();

    let missing = dir.path().join("zzzzzzz");
    let out = Grep
        .invoke(
            json!({
                "pattern": "anything",
                "path": missing.to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    // Error must exist but must NOT contain a suggestion.
    match out {
        ToolOutput::Error {
            recoverable,
            message,
        } => {
            assert!(recoverable);
            assert!(
                message.contains("path does not exist"),
                "expected path-not-found error, got: {message:?}"
            );
            assert!(
                !message.contains("Did you mean"),
                "expected no suggestion for dissimilar name, got: {message:?}"
            );
        }
        other => panic!("expected ToolOutput::Error, got {other:?}"),
    }
}

#[tokio::test]
async fn enoent_existing_path_unaffected() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("needle.txt"), "us003_marker\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "us003_marker",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.contains("needle.txt"),
        "existing path should search normally: {text:?}"
    );
}

// (batch 4): per-search timeout with env-knob override (gap #18).
//
// Thread-local overrides (TEST_DELAY_MS, TEST_TIMEOUT_MS) are used here
// instead of global atomics so each test's state is invisible to other tests
// running in parallel.  Env-var tests use only safe fallback values
// ("not_a_number", "0") that fall back to the 30 s default and cannot cause
// concurrent fast tests to time out.

#[tokio::test]
async fn timeout_under_budget_completes_normally() {
    // A trivially fast search must complete without hitting the 30 s default.
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "needle\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "needle",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(text.contains("a.txt"), "fast search must succeed: {text:?}");
}

#[tokio::test]
async fn timeout_env_var_triggers_error_and_message_has_seconds() {
    // Use thread-local overrides so this test is invisible to parallel tests:
    //   TEST_DELAY_MS=150  — async pre-search sleep simulates a slow invoke
    //   TEST_TIMEOUT_MS=50 — fires before the 150 ms sleep completes
    // The message uses timeout_secs from the env var ("60"), which is safe to
    // leak because no concurrent test takes more than 60 s.
    // Thread-local overrides: invisible to parallel tests.
    //   TEST_DELAY_MS=150  — async pre-search sleep, simulates a slow invoke
    //   TEST_TIMEOUT_MS=50 — fires before the 150 ms sleep completes
    // No env-var manipulation: timeout_secs falls back to DEFAULT_TIMEOUT_SECS
    // (30) so the message says "grep timed out after 30s".  Env-var override
    // parsing is tested separately.
    super::TEST_DELAY_MS.with(|c| c.set(150));
    super::TEST_TIMEOUT_MS.with(|c| c.set(50));

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "needle\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "needle",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();

    // Cleanup before any assertion that could panic.
    super::TEST_DELAY_MS.with(|c| c.set(0));
    super::TEST_TIMEOUT_MS.with(|c| c.set(0));

    // Error must be recoverable, contain the timeout phrase with the default
    // 30 s value, and be distinct from NO_MATCHES_MESSAGE.
    match out {
        ToolOutput::Error {
            recoverable,
            ref message,
        } => {
            assert!(recoverable, "timeout error must be recoverable");
            assert!(
                message.contains("grep timed out after 30s"),
                "expected timeout phrase with default seconds; got {message:?}"
            );
            assert!(
                !message.contains(NO_MATCHES_MESSAGE),
                "timeout error must be distinct from no-matches; got {message:?}"
            );
        }
        other => panic!("expected ToolOutput::Error, got {other:?}"),
    }
}

#[tokio::test]
async fn timeout_invalid_env_var_falls_back_to_default() {
    // A non-numeric env var must silently fall back to DEFAULT_TIMEOUT_SECS
    // and not error for a normal fast search.  "not_a_number" → 30 s default,
    // safe to leak to parallel tests.
    std::env::set_var("LAUNCHPAD_GREP_TIMEOUT_SECS", "not_a_number");

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "needle\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "needle",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();

    std::env::remove_var("LAUNCHPAD_GREP_TIMEOUT_SECS");

    let text = unwrap_text(out);
    assert!(
        text.contains("a.txt"),
        "invalid env var must not error; search must succeed: {text:?}"
    );
}

#[tokio::test]
async fn timeout_zero_env_var_falls_back_to_default() {
    // A zero value is explicitly invalid and must fall back to the default.
    // "0" → 30 s default, safe to leak to parallel tests.
    std::env::set_var("LAUNCHPAD_GREP_TIMEOUT_SECS", "0");

    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("a.txt"), "needle\n")
        .await
        .unwrap();

    let out = Grep
        .invoke(
            json!({
                "pattern": "needle",
                "path": dir.path().to_str().unwrap(),
            }),
            &ctx(),
        )
        .await
        .unwrap();

    std::env::remove_var("LAUNCHPAD_GREP_TIMEOUT_SECS");

    let text = unwrap_text(out);
    assert!(
        text.contains("a.txt"),
        "zero env var must not error; search must succeed: {text:?}"
    );
}

#[tokio::test]
async fn cancellation_still_works_under_timeout_wrapper() {
    // Pre-cancel the context; the cancellation check at the top of invoke
    // must still surface as AoError::Internal("cancelled") — not a timeout
    // error — even when the timeout wrapper is active.  Uses a thread-local
    // 200 ms override so this test is invisible to parallel tests.
    super::TEST_TIMEOUT_MS.with(|c| c.set(200));

    let dir = TempDir::new().unwrap();
    for i in 0..5 {
        tokio::fs::write(dir.path().join(format!("f_{i}.txt")), "needle\n")
            .await
            .unwrap();
    }

    let context = ctx();
    context.cancel.cancel();

    let path_s = dir.path().to_str().unwrap().to_string();
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        Grep.invoke(
            json!({
                "pattern": "needle",
                "path": path_s,
            }),
            &context,
        ),
    )
    .await
    .expect("must return within 100 ms");

    super::TEST_TIMEOUT_MS.with(|c| c.set(0));

    match result {
        Err(AoError::Internal(msg)) => assert_eq!(msg, CANCELLED_MESSAGE),
        other => panic!("expected AoError::Internal(\"cancelled\"), got {other:?}"),
    }
}

// Provider-coercion parity: integer and boolean arguments may arrive as JSON
// strings from some providers; the flag must still be honored rather than
// silently falling back to a default the model never requested.

#[test]
fn coerce_u64_accepts_number_and_string_forms() {
    assert_eq!(super::coerce_u64(&json!({ "k": 5 }), "k"), Some(5));
    assert_eq!(super::coerce_u64(&json!({ "k": "5" }), "k"), Some(5));
    assert_eq!(super::coerce_u64(&json!({ "k": " 5 " }), "k"), Some(5));
    // Unparseable / wrong-shape values yield None so the caller's default applies.
    assert_eq!(super::coerce_u64(&json!({ "k": "abc" }), "k"), None);
    assert_eq!(super::coerce_u64(&json!({ "k": -1 }), "k"), None);
    assert_eq!(super::coerce_u64(&json!({}), "k"), None);
}

#[test]
fn coerce_bool_accepts_bool_string_and_numeric_forms() {
    assert_eq!(super::coerce_bool(&json!({ "k": true }), "k"), Some(true));
    assert_eq!(super::coerce_bool(&json!({ "k": false }), "k"), Some(false));
    assert_eq!(super::coerce_bool(&json!({ "k": "true" }), "k"), Some(true));
    assert_eq!(super::coerce_bool(&json!({ "k": "TRUE" }), "k"), Some(true));
    assert_eq!(super::coerce_bool(&json!({ "k": "false" }), "k"), Some(false));
    assert_eq!(super::coerce_bool(&json!({ "k": "1" }), "k"), Some(true));
    assert_eq!(super::coerce_bool(&json!({ "k": "0" }), "k"), Some(false));
    assert_eq!(super::coerce_bool(&json!({ "k": 1 }), "k"), Some(true));
    assert_eq!(super::coerce_bool(&json!({ "k": 0 }), "k"), Some(false));
    // Unrecognized strings yield None so the caller's default applies.
    assert_eq!(super::coerce_bool(&json!({ "k": "maybe" }), "k"), None);
    assert_eq!(super::coerce_bool(&json!({}), "k"), None);
}

#[tokio::test]
async fn string_encoded_head_limit_is_honored() {
    let dir = TempDir::new().unwrap();
    for name in &["a.txt", "b.txt", "c.txt"] {
        tokio::fs::write(dir.path().join(name), "coerce_marker\n")
            .await
            .unwrap();
    }

    // head_limit arrives as the string "1": must truncate to a single result,
    // exactly as if the integer 1 had been sent.
    let out = Grep
        .invoke(
            json!({
                "pattern": "coerce_marker",
                "path": dir.path().to_str().unwrap(),
                "head_limit": "1",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    let lines: Vec<&str> = text.lines().collect();
    // summary + 1 path + footer
    assert_eq!(lines.len(), 3, "string head_limit must truncate; got {text:?}");
    assert_eq!(lines[0], "Found 1 file", "got {text:?}");
    assert_eq!(lines[2], "[paginated: limit=1 offset=0]", "got {text:?}");
}

#[tokio::test]
async fn string_encoded_case_insensitive_flag_is_honored() {
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("c.txt"), "COERCE_UPPER\n")
        .await
        .unwrap();

    // -i arrives as the string "true": a lowercase pattern must still match the
    // uppercase content. Without coercion this would silently stay
    // case-sensitive and find nothing.
    let out = Grep
        .invoke(
            json!({
                "pattern": "coerce_upper",
                "path": dir.path().to_str().unwrap(),
                "-i": "true",
            }),
            &ctx(),
        )
        .await
        .unwrap();
    let text = unwrap_text(out);
    assert!(
        text.contains("c.txt"),
        "string -i=\"true\" must enable case-insensitive match; got {text:?}"
    );
}
