use std::path::PathBuf;

use ao_protocol::changelog::ChangelogEntry;
use ao_protocol::error::AoError;
use ao_protocol::tasklist::TasklistOwner;

use crate::paths::DataRoot;

/// Append-only JSONL store for per-tasklist changelog entries.
///
/// Each line is a single serialized [`ChangelogEntry`]. The file lives under
/// the tasklist's own workspace directory with a leading-underscore name so
/// the outputs widget can filter it out — which workspace is decided by
/// ownership, exactly as for every other per-tasklist artifact:
/// [`DataRoot::tasklist_changelog_path`] for team-owned,
/// [`DataRoot::agent_tasklist_changelog_path`] for agent-owned.
///
/// Concurrency: writes go through `tokio::fs::OpenOptions::append(true)`,
/// which on POSIX gives atomic single-write semantics for any payload up
/// to `PIPE_BUF` (4 KiB on macOS/Linux). Each line is small (one entry's
/// JSON, well under that bound) so two concurrent appenders cannot
/// interleave bytes mid-line. This matches the convention already used by
/// [`crate::transcript::TranscriptStore::append`] — no file lock, no
/// temp+rename. Documented here rather than re-derived per call site.
pub struct ChangelogStore {
    data_root: DataRoot,
}

impl ChangelogStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Resolve the changelog file for a tasklist from its owner.
    fn path_for(&self, owner: &TasklistOwner, tasklist_id: &str) -> PathBuf {
        match owner {
            TasklistOwner::Team { team_id } => {
                self.data_root.tasklist_changelog_path(team_id, tasklist_id)
            }
            TasklistOwner::Agent { agent_id } => self
                .data_root
                .agent_tasklist_changelog_path(agent_id, tasklist_id),
        }
    }

    /// Where agent-owned changelogs were written before they became
    /// owner-aware. Both sides of the old code passed an empty string as the
    /// team id and `Path::join("")` collapses, so
    /// `{root}/teams/{team_id}/tasklists/{tasklist_id}/workspace/` resolved to
    /// `{root}/teams/tasklists/{tasklist_id}/workspace/_changelog.jsonl` —
    /// inside the legacy team subtree, under a phantom team directory named
    /// `tasklists`, detached from the tasklist's real workspace.
    ///
    /// Consulted on read only, and only when the correct path is absent, so
    /// installs created before the fix keep their task summaries in
    /// completion reports and co-pilot context. Nothing writes here any more.
    /// Safe to delete this fallback (and the stray directory) once no
    /// supported install predates the fix.
    fn legacy_agent_path(&self, tasklist_id: &str) -> PathBuf {
        self.data_root.tasklist_changelog_path("", tasklist_id)
    }

    /// Append one entry as a single JSON line to the tasklist's changelog
    /// file, creating parent directories on first write.
    pub async fn append(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        entry: &ChangelogEntry,
    ) -> Result<(), AoError> {
        let path = self.path_for(owner, tasklist_id);

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let line = serde_json::to_string(entry).map_err(|e| AoError::Json(e.to_string()))?;
        let line_with_newline = format!("{}\n", line);

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line_with_newline.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Return the last `n` entries in chronological order (oldest first).
    /// Empty vec when the file does not yet exist or is empty. Returns
    /// every entry when `n` exceeds the total count.
    ///
    /// Used by the co-pilot context-injection path to surface recent activity,
    /// and by the completion-report builder to source each task's summary.
    ///
    /// For agent-owned tasklists this reads [`Self::legacy_agent_path`] first
    /// and the owner-aware file second, concatenating both — see that method
    /// for why the legacy file exists.
    ///
    /// Concatenated rather than "whichever exists": on an upgraded install a
    /// tasklist can have history in the legacy file and new entries in the
    /// correct one, and preferring one file would make the other's entries
    /// vanish the moment the first new entry landed. Legacy entries are always
    /// older — nothing writes there any more — so legacy-then-current is
    /// chronological, which is what the `n` tail and the readers' "last entry
    /// per task wins" rule both depend on.
    pub async fn read_recent(
        &self,
        owner: &TasklistOwner,
        tasklist_id: &str,
        n: usize,
    ) -> Result<Vec<ChangelogEntry>, AoError> {
        let mut sources = Vec::with_capacity(2);
        if matches!(owner, TasklistOwner::Agent { .. }) {
            sources.push(self.legacy_agent_path(tasklist_id));
        }
        sources.push(self.path_for(owner, tasklist_id));

        let mut lines: Vec<String> = Vec::new();
        for path in sources {
            if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                continue;
            }
            let contents = tokio::fs::read_to_string(&path).await?;
            lines.extend(
                contents
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string),
            );
        }

        let start = lines.len().saturating_sub(n);
        let mut entries = Vec::with_capacity(lines.len() - start);
        for line in &lines[start..] {
            let entry: ChangelogEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn team(id: &str) -> TasklistOwner {
        TasklistOwner::Team {
            team_id: id.to_string(),
        }
    }

    fn agent(id: &str) -> TasklistOwner {
        TasklistOwner::Agent {
            agent_id: id.to_string(),
        }
    }

    fn make_entry(i: usize) -> ChangelogEntry {
        ChangelogEntry {
            task_id: format!("task-{i}"),
            tasklist_id: "tl-1".into(),
            agent_id: "agent-x".into(),
            status: "complete".into(),
            summary: format!("did thing {i}"),
            details: if i % 2 == 0 {
                Some(format!("more about thing {i}"))
            } else {
                None
            },
            // Spread timestamps so order assertions are independent of
            // wall-clock granularity.
            ts: Utc::now() + Duration::seconds(i as i64),
        }
    }

    #[tokio::test]
    async fn append_creates_workspace_dir_and_writes_jsonl_line() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root.clone());

        let path = data_root.tasklist_changelog_path("alpha", "tl-1");
        assert!(!tokio::fs::try_exists(&path).await.unwrap());

        let entry = make_entry(0);
        store.append(&team("alpha"), "tl-1", &entry).await.unwrap();

        // File now exists under workspace/, named _changelog.jsonl, with
        // exactly one JSON-line.
        assert!(tokio::fs::try_exists(&path).await.unwrap());
        assert_eq!(path.file_name().unwrap(), "_changelog.jsonl");
        let parent = path.parent().unwrap();
        assert_eq!(parent.file_name().unwrap(), "workspace");

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: ChangelogEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed, entry);
        assert!(contents.ends_with('\n'), "each entry must end with newline");
    }

    #[tokio::test]
    async fn append_then_tail_read_returns_last_n_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root);

        for i in 0..10 {
            store.append(&team("alpha"), "tl-1", &make_entry(i)).await.unwrap();
        }

        let recent = store.read_recent(&team("alpha"), "tl-1", 3).await.unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].task_id, "task-7");
        assert_eq!(recent[1].task_id, "task-8");
        assert_eq!(recent[2].task_id, "task-9");

        // Larger-than-total returns everything; chronological order preserved.
        let all = store.read_recent(&team("alpha"), "tl-1", 100).await.unwrap();
        assert_eq!(all.len(), 10);
        for (i, entry) in all.iter().enumerate() {
            assert_eq!(entry.task_id, format!("task-{i}"));
        }
    }

    #[tokio::test]
    async fn read_recent_on_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root);

        // No append yet → nothing on disk.
        let recent = store.read_recent(&team("alpha"), "tl-missing", 5).await.unwrap();
        assert!(recent.is_empty());
    }

    #[tokio::test]
    async fn entries_for_different_tasklists_do_not_leak() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root);

        store.append(&team("alpha"), "tl-A", &make_entry(0)).await.unwrap();
        store.append(&team("alpha"), "tl-A", &make_entry(1)).await.unwrap();
        store.append(&team("alpha"), "tl-B", &make_entry(99)).await.unwrap();

        let a = store.read_recent(&team("alpha"), "tl-A", 10).await.unwrap();
        let b = store.read_recent(&team("alpha"), "tl-B", 10).await.unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].task_id, "task-99");
        assert_ne!(a[0].task_id, b[0].task_id);
    }

    /// The defect this store's owner-awareness exists to prevent: an
    /// agent-owned changelog must land inside the tasklist's own workspace
    /// under `tasks/agents/`, and must not create anything under `teams/`.
    #[tokio::test]
    async fn agent_owned_append_writes_under_agent_tree_and_never_touches_teams() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root.clone());

        store
            .append(&agent("agent-1"), "tl-1", &make_entry(0))
            .await
            .unwrap();

        let expected = data_root.agent_tasklist_changelog_path("agent-1", "tl-1");
        assert!(
            tokio::fs::try_exists(&expected).await.unwrap(),
            "expected changelog at {}",
            expected.display()
        );
        // It sits beside the tasklist's other outputs, not in a parallel tree.
        assert_eq!(
            expected.parent().unwrap(),
            data_root.agent_tasklist_workspace_dir("agent-1", "tl-1")
        );
        assert!(
            !tokio::fs::try_exists(data_root.teams_dir()).await.unwrap(),
            "agent-owned append must not create the legacy teams/ subtree"
        );

        let read = store.read_recent(&agent("agent-1"), "tl-1", 10).await.unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].task_id, "task-0");
    }

    /// Two tasklists with the same id but different owners are distinct files.
    /// Before ownership was threaded through, every agent-owned tasklist
    /// collapsed onto a single `teams/`-rooted namespace keyed by tasklist id
    /// alone, so this pair could collide.
    #[tokio::test]
    async fn same_tasklist_id_under_different_owners_does_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ChangelogStore::new(DataRoot::new(tmp.path()));

        store.append(&agent("agent-1"), "tl-x", &make_entry(1)).await.unwrap();
        store.append(&agent("agent-2"), "tl-x", &make_entry(2)).await.unwrap();
        store.append(&team("team-1"), "tl-x", &make_entry(3)).await.unwrap();

        for (owner, expected_task) in [
            (agent("agent-1"), "task-1"),
            (agent("agent-2"), "task-2"),
            (team("team-1"), "task-3"),
        ] {
            let entries = store.read_recent(&owner, "tl-x", 10).await.unwrap();
            assert_eq!(entries.len(), 1, "owner {owner:?} saw another owner's entries");
            assert_eq!(entries[0].task_id, expected_task);
        }
    }

    /// Installs that predate the fix have their agent-owned changelogs at the
    /// collapsed `teams/`-rooted path. Reads must still find them, so upgrading
    /// does not silently empty completion reports and co-pilot context.
    #[tokio::test]
    async fn agent_owned_read_falls_back_to_the_legacy_collapsed_path() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root.clone());

        // Reproduce exactly what the old code wrote: empty team id, which
        // collapses to `{root}/teams/tasklists/{tasklist_id}/workspace/`.
        let legacy = data_root.tasklist_changelog_path("", "tl-old");
        assert_eq!(
            legacy,
            data_root
                .teams_dir()
                .join("tasklists")
                .join("tl-old")
                .join("workspace")
                .join("_changelog.jsonl"),
            "the legacy path this fallback targets must be the one the old code actually produced"
        );
        tokio::fs::create_dir_all(legacy.parent().unwrap()).await.unwrap();
        let line = serde_json::to_string(&make_entry(7)).unwrap();
        tokio::fs::write(&legacy, format!("{line}\n")).await.unwrap();

        let entries = store.read_recent(&agent("agent-1"), "tl-old", 10).await.unwrap();
        assert_eq!(entries.len(), 1, "legacy changelog was not read back");
        assert_eq!(entries[0].task_id, "task-7");

        // The fallback is read-only: a subsequent append goes to the new path.
        store.append(&agent("agent-1"), "tl-old", &make_entry(8)).await.unwrap();
        assert!(
            tokio::fs::try_exists(data_root.agent_tasklist_changelog_path("agent-1", "tl-old"))
                .await
                .unwrap()
        );

        // Both files now hold entries, and both must remain visible —
        // otherwise the first new entry would silently erase the tasklist's
        // history from completion reports. Legacy entries come first.
        let after = store.read_recent(&agent("agent-1"), "tl-old", 10).await.unwrap();
        assert_eq!(
            after.iter().map(|e| e.task_id.as_str()).collect::<Vec<_>>(),
            vec!["task-7", "task-8"],
            "legacy history must survive alongside new entries"
        );
    }

    /// The `n` tail must apply across the concatenation, not per file.
    #[tokio::test]
    async fn tail_limit_spans_legacy_and_current_files() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root.clone());

        let legacy = data_root.tasklist_changelog_path("", "tl-old");
        tokio::fs::create_dir_all(legacy.parent().unwrap()).await.unwrap();
        let mut buf = String::new();
        for i in 0..3 {
            buf.push_str(&serde_json::to_string(&make_entry(i)).unwrap());
            buf.push('\n');
        }
        tokio::fs::write(&legacy, buf).await.unwrap();

        for i in 3..6 {
            store.append(&agent("agent-1"), "tl-old", &make_entry(i)).await.unwrap();
        }

        let all = store.read_recent(&agent("agent-1"), "tl-old", 100).await.unwrap();
        assert_eq!(
            all.iter().map(|e| e.task_id.as_str()).collect::<Vec<_>>(),
            vec!["task-0", "task-1", "task-2", "task-3", "task-4", "task-5"]
        );

        // A tail that straddles the boundary takes the last 4 overall.
        let tail = store.read_recent(&agent("agent-1"), "tl-old", 4).await.unwrap();
        assert_eq!(
            tail.iter().map(|e| e.task_id.as_str()).collect::<Vec<_>>(),
            vec!["task-2", "task-3", "task-4", "task-5"]
        );
    }

    /// The fallback must not leak across ownership: a team-owned read never
    /// consults it, so a team literally named for a tasklist id cannot pick up
    /// another tasklist's legacy entries.
    #[tokio::test]
    async fn team_owned_read_does_not_consult_the_legacy_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = ChangelogStore::new(data_root.clone());

        let legacy = data_root.tasklist_changelog_path("", "tl-old");
        tokio::fs::create_dir_all(legacy.parent().unwrap()).await.unwrap();
        let line = serde_json::to_string(&make_entry(7)).unwrap();
        tokio::fs::write(&legacy, format!("{line}\n")).await.unwrap();

        let entries = store.read_recent(&team("team-1"), "tl-old", 10).await.unwrap();
        assert!(entries.is_empty());
    }
}
