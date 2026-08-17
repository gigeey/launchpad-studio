/// Configuration for instruction file patterns (e.g., CLAUDE.md, Cursor.md).
///
/// This pattern now holds a list of filenames rather than a single name so
/// the Instructions tab can surface every matching file in an agent home.
/// Matching is case-insensitive; the callers decide whether to pick the
/// first match or enumerate them all.

/// The default instruction file name used when no preference is configured.
pub const DEFAULT_INSTRUCTION_FILENAME: &str = "CLAUDE.md";

/// Instruction file pattern configuration.
#[derive(Debug, Clone)]
pub struct InstructionFilePattern {
    filenames: Vec<String>,
}

impl Default for InstructionFilePattern {
    fn default() -> Self {
        Self {
            filenames: vec![DEFAULT_INSTRUCTION_FILENAME.to_string()],
        }
    }
}

impl InstructionFilePattern {
    /// Create a pattern with an explicit list of filenames.
    pub fn new(filenames: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            filenames: filenames.into_iter().map(Into::into).collect(),
        }
    }

    /// Get the configured instruction filenames.
    pub fn filenames(&self) -> &[String] {
        &self.filenames
    }

    /// Resolve every configured filename against `dir`, returning one path
    /// per filename in the original order.
    pub fn resolve_all(&self, dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        self.filenames.iter().map(|f| dir.join(f)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_default_filenames() {
        let pattern = InstructionFilePattern::default();
        assert_eq!(pattern.filenames(), &["CLAUDE.md".to_string()]);
    }

    #[test]
    fn test_custom_filenames() {
        let pattern = InstructionFilePattern::new(["Cursor.md", "CLAUDE.md"]);
        assert_eq!(
            pattern.filenames(),
            &["Cursor.md".to_string(), "CLAUDE.md".to_string()]
        );
    }

    #[test]
    fn test_resolve_all_default() {
        let pattern = InstructionFilePattern::default();
        let resolved = pattern.resolve_all(Path::new("/home/user/project"));
        assert_eq!(resolved, vec![Path::new("/home/user/project/CLAUDE.md").to_path_buf()]);
    }

    #[test]
    fn test_resolve_all_multiple() {
        let pattern = InstructionFilePattern::new(["CLAUDE.md", "Cursor.md"]);
        let resolved = pattern.resolve_all(Path::new("/workspace"));
        assert_eq!(
            resolved,
            vec![
                Path::new("/workspace/CLAUDE.md").to_path_buf(),
                Path::new("/workspace/Cursor.md").to_path_buf(),
            ]
        );
    }
}
