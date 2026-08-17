//! Slugification of user-supplied titles into filename-safe strings.
//!
//! Two call sites share this: the skill-creation route in `ao-server`, which
//! derives a skill's folder name from its title, and rule-file naming in
//! `ao-engine`. The two must agree on the mapping, so it lives in one place.
//!
//! The worktree module in `ao-engine-tools-engine` has its own `slugify` with
//! deliberately different rules — it preserves dots, so a branch name like
//! `feat-1.2` survives intact. It is not a duplicate of this one.

/// Slugify a title into a filename-safe string.
///
/// Lowercases, replaces non-alphanumeric characters with hyphens,
/// collapses consecutive hyphens, and trims leading/trailing hyphens.
pub fn slugify(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // Collapse consecutive hyphens
    let mut result = String::with_capacity(slug.len());
    let mut prev_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("React Pattern"), "react-pattern");
    }

    #[test]
    fn test_slugify_special_chars() {
        assert_eq!(slugify("My Cool Skill!"), "my-cool-skill");
    }

    #[test]
    fn test_slugify_consecutive_specials() {
        assert_eq!(slugify("foo---bar  baz"), "foo-bar-baz");
    }

    #[test]
    fn test_slugify_leading_trailing() {
        assert_eq!(slugify("  hello  "), "hello");
    }

    #[test]
    fn test_slugify_numbers() {
        assert_eq!(slugify("React 18 Hooks"), "react-18-hooks");
    }

    #[test]
    fn test_slugify_empty() {
        assert_eq!(slugify("---"), "");
    }
}
