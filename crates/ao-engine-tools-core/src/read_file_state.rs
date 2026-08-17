use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// A snapshot of a file at the moment it was read by the `Read` tool.
///
/// `offset` and `limit` mirror the caller's requested view window.
/// When both are `None` the entry represents a complete file read and
/// `is_full_read()` returns `true`.
#[derive(Clone, Debug)]
pub struct ReadEntry {
    /// Raw decoded file contents (post-decode, pre-line-number formatting).
    pub content: String,
    /// File mtime at the time of the read. Used by the staleness check in
    /// `Edit` and `Write` to detect on-disk changes since the last read.
    pub mtime: SystemTime,
    /// Caller-supplied byte offset, if any.
    pub offset: Option<usize>,
    /// Caller-supplied line limit, if any.
    pub limit: Option<usize>,
    /// `true` when this snapshot's content was actually rendered to the model
    /// as `Read` output. `false` for snapshots recorded by `Edit`/`Write`/
    /// `NotebookEdit` after they mutate a file — those hold the new on-disk
    /// content, but the model has only seen an "updated" acknowledgement, not
    /// the content itself.
    ///
    /// The `Read` re-read dedup (which returns a lightweight "unchanged" stub
    /// instead of re-sending identical content) keys off this: it must never
    /// suppress content the model has not yet seen. A snapshot left behind by
    /// an edit therefore forces the next `Read` to surface the real content
    /// rather than the stub.
    pub surfaced_by_read: bool,
}

impl ReadEntry {
    /// Returns `true` when the entry represents a complete (non-windowed) read.
    ///
    /// A full read has neither an explicit offset nor an explicit limit. Only
    /// full-read entries allow the staleness fall-through path in `Edit`/`Write`
    /// (mtime advanced but bytes unchanged → allow the edit).
    pub fn is_full_read(&self) -> bool {
        self.offset.is_none() && self.limit.is_none()
    }

    /// Returns `true` when the entry covers only part of the file.
    ///
    /// The partial-read-then-write footgun: if the model has only seen a window
    /// of a file, it cannot safely overwrite the whole file. `Edit` and `Write`
    /// treat this as a hard error so the model must re-read the full file first.
    pub fn is_partial_view(&self) -> bool {
        !self.is_full_read()
    }
}

/// Session-wide map of per-file read snapshots.
///
/// Shared (via `Arc`) between parent and child `RunnerContext`s so that a
/// parent's read is visible to a child's `Edit`/`Write` without an extra
/// `Read` round-trip. The `Arc`-share semantics mirror `cwd`, `permissions`,
/// and `todos` on `RunnerContext`.
///
/// Lock discipline: the `Mutex` guard is held only for the duration of the
/// insert/clone — never returned from a method and never held across an `await`.
pub struct ReadFileState {
    inner: Mutex<HashMap<PathBuf, ReadEntry>>,
}

impl ReadFileState {
    /// Record (or overwrite) the read snapshot for `path`.
    ///
    /// Acquires and immediately drops the lock — never holds the guard outside
    /// this method.
    pub fn record(&self, path: PathBuf, entry: ReadEntry) {
        self.inner.lock().unwrap().insert(path, entry);
    }

    /// Return a clone of the snapshot for `path`, or `None` if not recorded.
    ///
    /// Acquires and immediately drops the lock before returning.
    pub fn get(&self, path: &Path) -> Option<ReadEntry> {
        self.inner.lock().unwrap().get(path).cloned()
    }
}

impl Default for ReadFileState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn make_entry(offset: Option<usize>, limit: Option<usize>) -> ReadEntry {
        ReadEntry {
            content: "hello".to_string(),
            mtime: SystemTime::UNIX_EPOCH,
            offset,
            limit,
            surfaced_by_read: true,
        }
    }

    #[test]
    fn record_then_get_returns_same_entry() {
        let state = ReadFileState::default();
        let path = PathBuf::from("/tmp/foo.txt");
        let entry = make_entry(None, None);
        state.record(path.clone(), entry.clone());
        let got = state.get(&path).expect("entry must be present after record");
        assert_eq!(got.content, entry.content);
        assert_eq!(got.offset, entry.offset);
        assert_eq!(got.limit, entry.limit);
    }

    #[test]
    fn record_overwrites_prior_entry_for_same_path() {
        let state = ReadFileState::default();
        let path = PathBuf::from("/tmp/foo.txt");
        state.record(path.clone(), make_entry(Some(0), Some(100)));
        let second = ReadEntry {
            content: "updated".to_string(),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            offset: None,
            limit: None,
            surfaced_by_read: true,
        };
        state.record(path.clone(), second.clone());
        let got = state.get(&path).unwrap();
        assert_eq!(got.content, "updated");
        assert_eq!(got.offset, None);
    }

    #[test]
    fn get_returns_none_for_unrecorded_path() {
        let state = ReadFileState::default();
        assert!(state.get(Path::new("/tmp/never-read.txt")).is_none());
    }

    #[test]
    fn is_full_read_true_when_no_offset_or_limit() {
        assert!(make_entry(None, None).is_full_read());
    }

    #[test]
    fn is_full_read_false_when_offset_set() {
        assert!(!make_entry(Some(0), None).is_full_read());
    }

    #[test]
    fn is_full_read_false_when_limit_set() {
        assert!(!make_entry(None, Some(2000)).is_full_read());
    }

    #[test]
    fn is_full_read_false_when_both_set() {
        assert!(!make_entry(Some(0), Some(2000)).is_full_read());
    }

    #[test]
    fn is_partial_view_is_negation_of_is_full_read() {
        let full = make_entry(None, None);
        let partial = make_entry(Some(0), Some(2000));
        assert!(!full.is_partial_view());
        assert!(partial.is_partial_view());
    }
}
