//! Command classifier for the Bash tool — UX enrichment only.
//!
//! # IMPORTANT: This is NOT a security boundary
//!
//! This module returns a [`Classification`] used to enrich permission-denial
//! prompts so the model can reason about the safety colour of a command.
//! It is **not** a security mechanism:
//!
//! - The permission grammar (`Bash(...)` rule matching) and hooks are the
//!   actual gating layer.
//! - The regex is best-effort and trivially fooled by obfuscation
//!   (e.g. `eval "rm -rf /"` or base64-encoded payloads).
//! - Do not rely on `Classification` to enforce safety under any
//!   circumstances.

use std::sync::LazyLock;

use regex::RegexSet;

/// Classification of a shell command for UX-enrichment purposes only.
///
/// See module-level docs for the security disclaimer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    ReadOnly,
    Destructive,
    NetworkTouching,
    GitMutating,
    Unclassified,
}

// Pattern sets, in priority order: GitMutating > Destructive > NetworkTouching > ReadOnly.
// First match wins; no match → Unclassified.

static GIT_MUTATING: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"\bgit\s+push\b",
        r"\bgit\s+reset\s+--hard\b",
        r"\bgit\s+rebase\b",
        r"\bgit\s+push\s+-f\b",
        r"\bgit\s+push\s+--force\b",
        r"\bgit\s+(push|reset|rebase|checkout)\b.*--force",
        r"\bgit\s+branch\s+-D\b",
    ])
    .expect("GIT_MUTATING regex set is valid")
});

static DESTRUCTIVE: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"\brm\s+-[rR]?[fF]\b|\brm\s+-[fF][rR]?\b",
        r">>?\s*/(etc|var|usr|sys|proc)/",
        r"\btruncate\b",
        r"\bshred\b",
        r"\bdd\s+if=",
        r"\bmkfs\b",
        r"\b:>\s*/",
    ])
    .expect("DESTRUCTIVE regex set is valid")
});

static NETWORK_TOUCHING: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"\bcurl\b",
        r"\bwget\b",
        r"\bnc\s",
        r"\bnetcat\b",
        r"\bssh\s",
        r"\bscp\s",
        r"\brsync\b",
        r"\bsftp\s",
        r"\bping\s",
    ])
    .expect("NETWORK_TOUCHING regex set is valid")
});

static READ_ONLY: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"\bls\b",
        r"\bcat\b",
        r"\bhead\b",
        r"\btail\b",
        r"\bgrep\b",
        r"\brg\b",
        r"\bfind\b",
        r"\bstat\b",
        r"\bfile\b",
        r"\bwc\b",
        r"\bsort\b",
        r"\buniq\b",
        r"\bawk\b",
        r"\bjq\b",
        r"\byq\b",
        r"\bless\b",
        r"\bmore\b",
        r"\btree\b",
        r"\bdu\b",
        r"\bdf\b",
        r"\benv\b",
        r"\bprintenv\b",
        r"\bdate\b",
        r"\becho\b",
        r"\bpwd\b",
        r"\bwhoami\b",
        r"\bid\b",
        r"\bhostname\b",
        // Read-only git verbs. Mutating git invocations are matched earlier
        // by GIT_MUTATING (priority order GitMutating > … > ReadOnly), so a
        // command like `git push` lands in GitMutating before reaching here.
        r"\bgit\s+status\b",
        r"\bgit\s+diff\b",
        r"\bgit\s+log\b",
        r"\bgit\s+show\b",
        r"\bgit\s+blame\b",
        r"\bgit\s+ls-files\b",
        r"\bgit\s+ls-remote\b",
        r"\bgit\s+rev-parse\b",
        r"\bgit\s+describe\b",
    ])
    .expect("READ_ONLY regex set is valid")
});

/// Classify `command` by scanning for known patterns.
///
/// Called on the **post-cd-lift** command string (i.e. after
/// [`super::execute::split_leading_cd`] has stripped any leading `cd <path> &&`).
/// Priority order: GitMutating > Destructive > NetworkTouching > ReadOnly.
/// If no pattern matches, returns [`Classification::Unclassified`].
pub fn classify(command: &str) -> Classification {
    if GIT_MUTATING.is_match(command) {
        return Classification::GitMutating;
    }
    if DESTRUCTIVE.is_match(command) {
        return Classification::Destructive;
    }
    if NETWORK_TOUCHING.is_match(command) {
        return Classification::NetworkTouching;
    }
    if READ_ONLY.is_match(command) {
        return Classification::ReadOnly;
    }
    Classification::Unclassified
}

#[cfg(test)]
mod tests {
    use super::{classify, Classification};

    // --- GitMutating ---

    #[test]
    fn git_push_is_mutating() {
        assert_eq!(
            classify("git push origin main"),
            Classification::GitMutating
        );
    }

    #[test]
    fn git_push_force_flag_is_mutating() {
        assert_eq!(
            classify("git push -f origin main"),
            Classification::GitMutating
        );
    }

    #[test]
    fn git_push_force_long_flag_is_mutating() {
        assert_eq!(
            classify("git push --force origin main"),
            Classification::GitMutating
        );
    }

    #[test]
    fn git_reset_hard_is_mutating() {
        assert_eq!(
            classify("git reset --hard HEAD~1"),
            Classification::GitMutating
        );
    }

    #[test]
    fn git_rebase_is_mutating() {
        assert_eq!(classify("git rebase main"), Classification::GitMutating);
    }

    #[test]
    fn git_branch_delete_is_mutating() {
        assert_eq!(
            classify("git branch -D old-branch"),
            Classification::GitMutating
        );
    }

    #[test]
    fn git_checkout_force_is_mutating() {
        assert_eq!(
            classify("git checkout --force feature"),
            Classification::GitMutating
        );
    }

    // --- Destructive ---

    #[test]
    fn rm_rf_is_destructive() {
        assert_eq!(classify("rm -rf /tmp/foo"), Classification::Destructive);
    }

    #[test]
    fn rm_fr_is_destructive() {
        assert_eq!(classify("rm -fr /tmp/foo"), Classification::Destructive);
    }

    #[test]
    fn rm_f_is_destructive() {
        assert_eq!(classify("rm -f somefile"), Classification::Destructive);
    }

    #[test]
    fn redirect_to_etc_is_destructive() {
        assert_eq!(
            classify("echo x >> /etc/passwd"),
            Classification::Destructive
        );
    }

    #[test]
    fn truncate_is_destructive() {
        assert_eq!(
            classify("truncate -s 0 somefile"),
            Classification::Destructive
        );
    }

    #[test]
    fn shred_is_destructive() {
        assert_eq!(
            classify("shred -u secrets.txt"),
            Classification::Destructive
        );
    }

    #[test]
    fn dd_if_is_destructive() {
        assert_eq!(
            classify("dd if=/dev/zero of=/dev/sda"),
            Classification::Destructive
        );
    }

    #[test]
    fn mkfs_is_destructive() {
        assert_eq!(classify("mkfs.ext4 /dev/sdb1"), Classification::Destructive);
    }

    // --- NetworkTouching ---

    #[test]
    fn curl_is_network() {
        assert_eq!(
            classify("curl https://example.com"),
            Classification::NetworkTouching
        );
    }

    #[test]
    fn wget_is_network() {
        assert_eq!(
            classify("wget https://example.com"),
            Classification::NetworkTouching
        );
    }

    #[test]
    fn ssh_is_network() {
        assert_eq!(classify("ssh user@host"), Classification::NetworkTouching);
    }

    #[test]
    fn scp_is_network() {
        assert_eq!(
            classify("scp file.txt user@host:/tmp/"),
            Classification::NetworkTouching
        );
    }

    #[test]
    fn rsync_is_network() {
        assert_eq!(
            classify("rsync -av src/ dst/"),
            Classification::NetworkTouching
        );
    }

    #[test]
    fn ping_is_network() {
        assert_eq!(
            classify("ping -c 1 8.8.8.8"),
            Classification::NetworkTouching
        );
    }

    #[test]
    fn nc_space_is_network() {
        assert_eq!(classify("nc -z host 80"), Classification::NetworkTouching);
    }

    #[test]
    fn netcat_is_network() {
        assert_eq!(classify("netcat host 80"), Classification::NetworkTouching);
    }

    // --- ReadOnly ---

    #[test]
    fn ls_is_readonly() {
        assert_eq!(classify("ls /tmp"), Classification::ReadOnly);
    }

    #[test]
    fn cat_is_readonly() {
        assert_eq!(classify("cat /etc/hosts"), Classification::ReadOnly);
    }

    #[test]
    fn grep_is_readonly() {
        assert_eq!(classify("grep -r 'pattern' src/"), Classification::ReadOnly);
    }

    #[test]
    fn find_is_readonly() {
        assert_eq!(classify("find . -name '*.rs'"), Classification::ReadOnly);
    }

    #[test]
    fn echo_is_readonly() {
        assert_eq!(classify("echo hello"), Classification::ReadOnly);
    }

    #[test]
    fn pwd_is_readonly() {
        assert_eq!(classify("pwd"), Classification::ReadOnly);
    }

    #[test]
    fn jq_is_readonly() {
        assert_eq!(classify("jq '.foo' data.json"), Classification::ReadOnly);
    }

    #[test]
    fn git_diff_is_readonly() {
        assert_eq!(classify("git diff"), Classification::ReadOnly);
        assert_eq!(classify("git diff HEAD~1"), Classification::ReadOnly);
    }

    #[test]
    fn git_status_is_readonly() {
        assert_eq!(classify("git status"), Classification::ReadOnly);
    }

    #[test]
    fn git_log_is_readonly() {
        assert_eq!(classify("git log --oneline"), Classification::ReadOnly);
    }

    #[test]
    fn git_show_is_readonly() {
        assert_eq!(classify("git show HEAD"), Classification::ReadOnly);
    }

    #[test]
    fn git_blame_is_readonly() {
        assert_eq!(classify("git blame src/main.rs"), Classification::ReadOnly);
    }

    #[test]
    fn git_rev_parse_is_readonly() {
        assert_eq!(
            classify("git rev-parse --show-toplevel"),
            Classification::ReadOnly
        );
    }

    /// Mutating verbs win over ReadOnly even if both patterns would match
    /// elsewhere in the same pipeline — priority is GitMutating > ReadOnly.
    #[test]
    fn git_diff_piped_into_push_is_mutating() {
        assert_eq!(
            classify("git diff | tee /tmp/x && git push origin main"),
            Classification::GitMutating
        );
    }

    // --- Unclassified ---

    #[test]
    fn unknown_command_is_unclassified() {
        assert_eq!(classify("myprogam --flag"), Classification::Unclassified);
    }

    #[test]
    fn complex_pipeline_no_known_cmds_is_unclassified() {
        assert_eq!(
            classify("custom_tool | other_tool --arg"),
            Classification::Unclassified
        );
    }

    #[test]
    fn empty_string_is_unclassified() {
        assert_eq!(classify(""), Classification::Unclassified);
    }

    // --- Priority: higher-risk wins over lower-risk in a pipeline ---

    /// `cat /tmp/foo | rm -rf /tmp/bar` → Destructive wins over ReadOnly.
    #[test]
    fn pipeline_destructive_wins_over_readonly() {
        assert_eq!(
            classify("cat /tmp/foo | rm -rf /tmp/bar"),
            Classification::Destructive
        );
    }

    /// `ls | curl https://example.com` → NetworkTouching wins over ReadOnly.
    #[test]
    fn pipeline_network_wins_over_readonly() {
        assert_eq!(
            classify("ls | curl https://example.com"),
            Classification::NetworkTouching
        );
    }

    /// `curl … | git push` → GitMutating wins over NetworkTouching.
    #[test]
    fn pipeline_git_mutating_wins_over_network() {
        assert_eq!(
            classify("curl https://example.com | git push origin main"),
            Classification::GitMutating
        );
    }

    // --- Negative / quoted cases ---

    /// The word "rm" inside a quoted string should still be classified as
    /// Destructive because the classifier is regex-based, not syntax-aware.
    /// But a string like `echo "just a string with rm -rf in it"` does contain
    /// the pattern — document this known limitation. The test below confirms
    /// the classifier doesn't false-positive on a command that only has "rm"
    /// as part of a word but not as `rm -f/-r`.
    #[test]
    fn word_containing_rm_not_classified_destructive() {
        // "remove" contains "rm" but there's no `rm -f` / `rm -r` pattern.
        assert_ne!(classify("remove old_files"), Classification::Destructive);
    }

    #[test]
    fn git_log_is_not_mutating() {
        // git log doesn't match any mutating pattern.
        assert_ne!(classify("git log --oneline"), Classification::GitMutating);
    }

    #[test]
    fn git_status_is_not_mutating() {
        assert_ne!(classify("git status"), Classification::GitMutating);
    }

    #[test]
    fn git_diff_is_not_mutating() {
        assert_ne!(classify("git diff HEAD"), Classification::GitMutating);
    }

    #[test]
    fn nc_without_space_is_not_network() {
        // "ncurses" contains "nc" but the pattern requires `\bnc\s` (space after nc).
        assert_ne!(
            classify("ncurses-config --version"),
            Classification::NetworkTouching
        );
    }
}
