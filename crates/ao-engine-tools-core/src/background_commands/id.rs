use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

// Process-wide counter; ids are unique across the process lifetime so the
// model can distinguish commands from different sessions without ambiguity.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Human-friendly identifier for a live background shell command.
///
/// IDs are sequential strings like `bash_1`, `bash_2`, etc., minted from a
/// process-wide atomic counter. They are easier to reference in logs and model
/// output than raw UUIDs while still being unique within a process lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackgroundCommandId(String);

impl BackgroundCommandId {
    /// Mint a fresh id. IDs are monotonically increasing within the process.
    pub fn new() -> Self {
        let n = NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1;
        Self(format!("bash_{n}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for BackgroundCommandId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BackgroundCommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BackgroundCommandId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for BackgroundCommandId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
