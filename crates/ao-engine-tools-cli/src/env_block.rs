//! Renders a `<cli-environment>` block for the system prompt so the
//! model has the context it needs to issue tool calls correctly:
//! absolute file paths (Read, Edit, Write all require them), shell
//! commands aware of the host OS, and reasonable assumptions about
//! `git` availability.
//!
//! The block is plain text inside an XML-shaped wrapper. The runner
//! does not parse it — the model treats it as part of the system
//! prompt.

use std::path::Path;

/// Build the `<cli-environment>` block describing the process's runtime
/// surface. `cwd` is taken explicitly so callers can render a block for
/// a directory other than the process's cwd (e.g. tests, future
/// per-session overrides).
pub fn render(cwd: &Path) -> String {
    let cwd_display = cwd.display().to_string();
    let platform = platform_label();
    let os_family = std::env::consts::FAMILY;
    let os_arch = std::env::consts::ARCH;
    let shell = std::env::var("SHELL")
        .ok()
        .and_then(|s| {
            Path::new(&s)
                .file_name()
                .and_then(|os| os.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let date = current_date_utc();
    let is_git_repo = looks_like_git_repo(cwd);

    format!(
        "<cli-environment>\n\
         - cwd: {cwd_display}\n\
         - platform: {platform}\n\
         - os_family: {os_family}\n\
         - arch: {os_arch}\n\
         - shell: {shell}\n\
         - date: {date}\n\
         - is_git_repo: {is_git_repo}\n\
         </cli-environment>\n\n\
         File-touching tools (Read, Edit, Write) require absolute paths. \
         When the user names a file relatively, resolve it against `cwd` \
         above before calling the tool.",
    )
}

/// Friendly label for the OS — `darwin` / `linux` / `windows` for the
/// common cases, falling back to whatever `std::env::consts::OS`
/// reports for everything else.
fn platform_label() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Walk up from `start` looking for a `.git` directory or file
/// (worktrees use a `.git` file pointing at the real gitdir). Stops at
/// the filesystem root. Returns `false` if any I/O error makes the
/// answer ambiguous — a missing `.git` is the safe default.
fn looks_like_git_repo(start: &Path) -> bool {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(".git");
        if candidate.exists() {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// Render the current UTC date in `YYYY-MM-DD` form. Done with manual
/// arithmetic because the CLI crate intentionally avoids a `chrono`
/// dependency just for one line in a system prompt.
fn current_date_utc() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = epoch_seconds_to_ymd(now as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert UTC seconds-since-epoch into `(year, month, day)`. The
/// algorithm is the standard "civil from days" formulation by Howard
/// Hinnant — exact for any year supported by `i64` seconds.
fn epoch_seconds_to_ymd(secs: i64) -> (i32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn render_includes_cwd_platform_and_block_tags() {
        let tmp = TempDir::new().unwrap();
        let block = render(tmp.path());
        assert!(block.starts_with("<cli-environment>"), "got: {block}");
        assert!(block.contains("</cli-environment>"), "got: {block}");
        assert!(
            block.contains(&format!("- cwd: {}", tmp.path().display())),
            "got: {block}"
        );
        assert!(block.contains("absolute paths"), "got: {block}");
    }

    #[test]
    fn render_detects_git_repo_when_dot_git_exists() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let block = render(tmp.path());
        assert!(block.contains("is_git_repo: true"), "got: {block}");
    }

    #[test]
    fn render_detects_git_repo_for_nested_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let block = render(&nested);
        assert!(block.contains("is_git_repo: true"), "got: {block}");
    }

    #[test]
    fn render_reports_no_git_repo_when_absent() {
        let tmp = TempDir::new().unwrap();
        let block = render(tmp.path());
        assert!(block.contains("is_git_repo: false"), "got: {block}");
    }

    #[test]
    fn epoch_seconds_to_ymd_known_dates() {
        // 2024-01-01 00:00:00 UTC = 1_704_067_200
        assert_eq!(epoch_seconds_to_ymd(1_704_067_200), (2024, 1, 1));
        // 1970-01-01 00:00:00 UTC = 0
        assert_eq!(epoch_seconds_to_ymd(0), (1970, 1, 1));
        // 2000-02-29 (leap day) 00:00:00 UTC = 951_782_400
        assert_eq!(epoch_seconds_to_ymd(951_782_400), (2000, 2, 29));
    }

    #[test]
    fn current_date_utc_has_expected_shape() {
        let s = current_date_utc();
        assert_eq!(s.len(), 10, "got: {s}");
        assert!(&s[4..5] == "-" && &s[7..8] == "-", "got: {s}");
        let _: u32 = s[0..4].parse().expect("year is numeric");
        let _: u32 = s[5..7].parse().expect("month is numeric");
        let _: u32 = s[8..10].parse().expect("day is numeric");
    }
}
