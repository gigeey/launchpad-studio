//! Conservative auto-approval gate for Bash commands.
//!
//! This module answers a single question: can a given Bash command be executed
//! *without* surfacing a human permission prompt? The answer is yes only when the
//! command is provably read-only and structurally safe — no shell-injection vectors,
//! no execution-smuggling constructs, and every pipe segment limited to a strict
//! allowlist of known-safe commands.
//!
//! # Design philosophy
//!
//! This is an allowlist gate, not a full shell parser. It operates on the raw
//! command string and looks for the presence of dangerous substrings. False
//! negatives (asking when we could have allowed) are harmless — the user sees
//! one extra prompt. False positives (silently running something that mutates
//! state or escapes the shell) are the class of bug this module exists to
//! prevent. When in doubt, reject.
//!
//! # What gets rejected automatically
//!
//! - Command substitution: `$(...)` and backtick forms.
//! - Process substitution: `<(...)` and `>(...)`.
//! - Output redirection: any `>` character (conservatively blocks `>`, `>>`,
//!   `&>`, `1>`, `2>`, etc.).
//! - Sequencing / conditional operators: `;`, `&&`, `||`.
//! - Background control: `&`.
//! - Multi-line scripts: embedded newlines.
//! - Commands not on the explicit allowlist (see [`SAFE_COMMANDS`]).
//!
//! # What is allowed through
//!
//! Pipelines of safe read-only commands, bare variable expansions (`$VAR`), input
//! redirection (`<`), and brace grouping without any of the above hazards.
//!
//! # Relationship to the command classifier
//!
//! [`super::command_classifier`] labels commands for UX colouring and is
//! intentionally permissive. This module is stricter and is the actual gate for
//! skipping the permission prompt. Commands on the classifier's read-only list
//! (e.g. `awk`, `find`, `sort`) may still be excluded here when they have
//! file-write or shell-exec escape hatches.

use std::collections::HashSet;
use std::sync::LazyLock;

/// Commands that are safe to auto-approve as head (or only) command of a pipe
/// segment. Every entry here is known to have no file-write or shell-exec
/// escape hatch under normal flag usage.
///
/// # Commands deliberately excluded (and why)
///
/// - `awk`        — `print > file` writes files; `system()` execs arbitrary programs.
/// - `find`       — `-exec`, `-delete`, `-fprintf` mutate state or run programs.
/// - `sort`       — `-o FILE` writes output to a file; `--compress-program` runs a program.
/// - `env`        — its primary purpose is to run a program (`env rm -rf /` works).
/// - `less`/`more`— pagers with `LESSOPEN`/shell-escape hooks; also pointless for an agent.
/// - `sed`        — `-i` edits files in place.
/// - `jq`/`yq`   — no exec escapes, stdout only, but excluded for conservatism at this tier.
///
/// All excluded commands still work; they simply require the normal permission
/// prompt rather than being auto-approved.
static SAFE_COMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "ls", "cat", "head", "tail", "grep", "rg", "stat", "file", "wc", "pwd", "echo",
        "whoami", "id", "hostname", "date", "printenv", "tree", "df", "du", "uniq",
        "basename", "dirname", "realpath", "which", "type", "cut", "nl", "column", "comm",
        "cksum", "md5sum", "sha1sum", "sha256sum", "true", "false",
    ]
    .into_iter()
    .collect()
});

/// Read-only `git` subcommands that are safe to auto-approve.
static SAFE_GIT_SUBCOMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "status", "diff", "log", "show", "blame", "ls-files", "ls-remote", "rev-parse",
        "describe",
    ]
    .into_iter()
    .collect()
});

/// Returns `true` only if `full_command` is safe to auto-approve without a
/// human prompt. `full_command` is the ORIGINAL command string exactly as the
/// model supplied it (including any leading `cd <path> && ...`).
pub fn is_auto_approvable(full_command: &str) -> bool {
    // Step 1: strip a leading `cd <path> && …` or `cd <path> ; …` prefix.
    let (cd_path, rest) = crate::bash::execute::split_leading_cd(full_command);

    // Step 2: reject if the cd target itself contains a command substitution.
    // Even though split_leading_cd removes the `cd … &&` separator, bash would
    // still evaluate `$(…)` or a backtick inside the cd argument at runtime.
    if let Some(p) = cd_path {
        if p.contains("$(") || p.contains('`') {
            return false;
        }
    }

    // Step 3: reject the remainder if it contains any execution-smuggling construct.
    // Check order: more-specific patterns first, then the bare `&` catch-all last.
    if rest.contains("$(") || rest.contains('`') {
        return false; // command substitution
    }
    if rest.contains("<(") || rest.contains(">(") {
        return false; // process substitution
    }
    if rest.contains('>') {
        return false; // output redirection (covers >, >>, &>, 1>, 2>)
    }
    if rest.contains(';') {
        return false; // command sequencing
    }
    if rest.contains("&&") || rest.contains("||") {
        return false; // conditional chaining
    }
    // Any remaining `&` is a standalone background operator (&&/&> already
    // rejected above, so this only fires for bare `&`).
    if rest.contains('&') {
        return false;
    }
    if rest.contains('\n') {
        return false; // multi-line script
    }

    // Step 4: split on `|` (safe: `||` was already rejected) and verify each segment.
    rest.split('|').all(|seg| segment_is_safe(seg.trim()))
}

/// Returns `true` if `seg` (a single pipe segment, already trimmed) is headed
/// by a command on the auto-approve allowlist.
fn segment_is_safe(seg: &str) -> bool {
    let mut tokens = seg.split_ascii_whitespace();
    let head = match tokens.next() {
        Some(h) => h,
        None => return false, // empty segment
    };

    // Assignment prefix like `FOO=bar cmd` is not safe: it can prefix any command.
    if head.contains('=') {
        return false;
    }

    if head == "git" {
        // git is only safe with a known read-only subcommand.
        let sub = match tokens.next() {
            Some(s) => s,
            None => return false,
        };
        return SAFE_GIT_SUBCOMMANDS.contains(sub);
    }

    SAFE_COMMANDS.contains(head)
}

#[cfg(test)]
mod tests {
    use super::is_auto_approvable;

    // --- ALLOW ---

    #[test]
    fn allow_ls() {
        assert!(is_auto_approvable("ls"));
    }

    #[test]
    fn allow_ls_with_flags_and_path() {
        assert!(is_auto_approvable("ls -la /tmp"));
    }

    #[test]
    fn allow_cat_etc_hosts() {
        assert!(is_auto_approvable("cat /etc/hosts"));
    }

    #[test]
    fn allow_grep_recursive() {
        assert!(is_auto_approvable("grep -r foo src/"));
    }

    #[test]
    fn allow_pwd() {
        assert!(is_auto_approvable("pwd"));
    }

    #[test]
    fn allow_echo() {
        assert!(is_auto_approvable("echo hi"));
    }

    #[test]
    fn allow_git_status() {
        assert!(is_auto_approvable("git status"));
    }

    #[test]
    fn allow_git_diff() {
        assert!(is_auto_approvable("git diff HEAD~1"));
    }

    #[test]
    fn allow_git_log() {
        assert!(is_auto_approvable("git log --oneline"));
    }

    #[test]
    fn allow_readonly_pipeline() {
        assert!(is_auto_approvable("grep foo file | head -n 20 | wc -l"));
    }

    #[test]
    fn allow_leading_cd() {
        assert!(is_auto_approvable("cd /tmp && ls"));
    }

    // --- REJECT (footguns) ---

    #[test]
    fn reject_command_substitution_dollar_paren() {
        assert!(!is_auto_approvable("echo $(rm -rf /)"));
    }

    #[test]
    fn reject_command_substitution_backtick() {
        assert!(!is_auto_approvable("echo `rm -rf /`"));
    }

    #[test]
    fn reject_chaining_and() {
        assert!(!is_auto_approvable("ls && rm -rf /"));
    }

    #[test]
    fn reject_sequencing_semicolon() {
        assert!(!is_auto_approvable("ls; rm -rf /"));
    }

    #[test]
    fn reject_conditional_or() {
        assert!(!is_auto_approvable("ls || rm -rf /"));
    }

    #[test]
    fn reject_output_redirect() {
        assert!(!is_auto_approvable("cat f > /etc/passwd"));
    }

    #[test]
    fn reject_append_redirect() {
        assert!(!is_auto_approvable("cat f >> out"));
    }

    #[test]
    fn reject_stderr_redirect() {
        assert!(!is_auto_approvable("grep foo 2>/dev/null"));
    }

    #[test]
    fn reject_process_substitution() {
        assert!(!is_auto_approvable("diff <(a) <(b)"));
    }

    #[test]
    fn reject_background_operator() {
        assert!(!is_auto_approvable("ls &"));
    }

    #[test]
    fn reject_unknown_pipe_segment() {
        assert!(!is_auto_approvable("ls | unknown_tool"));
    }

    #[test]
    fn reject_cd_target_substitution() {
        // Key footgun: split_leading_cd strips the separator but bash still
        // evaluates the substitution inside the cd argument.
        assert!(!is_auto_approvable("cd \"$(evil)\" && ls"));
    }

    #[test]
    fn reject_awk_excluded() {
        assert!(!is_auto_approvable("awk 'BEGIN{system(\"rm -rf /\")}'"));
    }

    #[test]
    fn reject_find_excluded() {
        assert!(!is_auto_approvable("find . -delete"));
    }

    #[test]
    fn reject_sort_excluded() {
        assert!(!is_auto_approvable("sort -o /etc/x f"));
    }

    #[test]
    fn reject_env_excluded() {
        assert!(!is_auto_approvable("env rm -rf /"));
    }

    #[test]
    fn reject_assignment_prefix() {
        assert!(!is_auto_approvable("FOO=bar ls"));
    }

    #[test]
    fn reject_git_push() {
        assert!(!is_auto_approvable("git push origin main"));
    }

    #[test]
    fn reject_rm_destructive() {
        assert!(!is_auto_approvable("rm -rf /tmp/x"));
    }

    #[test]
    fn reject_curl_network() {
        assert!(!is_auto_approvable("curl http://x"));
    }

    #[test]
    fn reject_unknown_command() {
        assert!(!is_auto_approvable("myunknown --flag"));
    }

    #[test]
    fn reject_empty_string() {
        assert!(!is_auto_approvable(""));
    }
}
