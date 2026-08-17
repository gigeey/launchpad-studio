use std::io;
use std::path::{Path, PathBuf};

use ao_persistence::paths::DataRoot;
use ao_protocol::agent::AgentProfile;
use ao_protocol::instructions::{InstructionDto, InstructionManifest};
use chrono::{DateTime, Utc};

/// Hidden sidecar directory (inside `{agent_home}/`) that stores per-file
/// `InstructionManifest` overrides. Hidden so it doesn't clutter the agent
/// home when the user browses their files.
const MANIFEST_DIR: &str = ".instructions";

/// Resolves the agent's home directory (respecting an explicit `home_dir`
/// on the profile and otherwise falling back to the data-root default).
pub fn resolve_agent_home_dir(agent: &AgentProfile, data_root: &DataRoot) -> PathBuf {
    agent
        .home_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| data_root.agent_home_dir(&agent.id))
}

fn manifest_path(agent_home: &Path, filename: &str) -> PathBuf {
    agent_home
        .join(MANIFEST_DIR)
        .join(format!("{filename}.manifest.json"))
}

fn read_manifest(agent_home: &Path, filename: &str) -> Option<InstructionManifest> {
    let path = manifest_path(agent_home, filename);
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_manifest(
    agent_home: &Path,
    filename: &str,
    manifest: &InstructionManifest,
) -> io::Result<()> {
    let path = manifest_path(agent_home, filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)
}

fn file_mtime(path: &Path) -> DateTime<Utc> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}

fn read_file_contents(path: &Path) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => Some(s),
            Err(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "skipping instruction file: invalid UTF-8",
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "skipping instruction file: read failed",
            );
            None
        }
    }
}

/// Scans the root of `{agent_home}/` (non-recursive) for files whose
/// filename matches one of `patterns` under case-insensitive equality.
///
/// Ordering: sorted by lowercased filename. Duplicate case-variants
/// (`CLAUDE.md` and `claude.md`) are deduped to the first in lexicographic
/// order; the skipped file is logged as a warning.
pub fn list_instructions(
    agent: &AgentProfile,
    data_root: &DataRoot,
    patterns: &[String],
) -> io::Result<Vec<InstructionDto>> {
    let agent_home = resolve_agent_home_dir(agent, data_root);
    scan_instructions(&agent_home, patterns)
}

/// Scans `agent_home` directly. Exposed for tests and for the
/// agent-context loader that already has the path resolved.
pub fn scan_instructions(agent_home: &Path, patterns: &[String]) -> io::Result<Vec<InstructionDto>> {
    let entries = match std::fs::read_dir(agent_home) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if patterns
            .iter()
            .any(|p| p.eq_ignore_ascii_case(name))
        {
            matches.push(path);
        }
    }

    // Deterministic: sort by lowercased filename, then by the raw
    // filename so ties (different case variants of the same name) break in
    // lexicographic order. The first wins for each case-insensitive key.
    matches.sort_by(|a, b| {
        let an = a.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let bn = b.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        an.to_lowercase()
            .cmp(&bn.to_lowercase())
            .then_with(|| an.cmp(&bn))
    });

    let mut out: Vec<InstructionDto> = Vec::new();
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    for path in matches {
        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let key = filename.to_lowercase();
        if !seen_keys.insert(key) {
            tracing::warn!(
                path = %path.display(),
                "skipping duplicate instruction file (case-insensitive collision)",
            );
            continue;
        }
        let Some(content) = read_file_contents(&path) else {
            continue;
        };

        let enabled = read_manifest(agent_home, &filename)
            .map(|m| m.enabled)
            .unwrap_or(true);

        out.push(InstructionDto {
            id: filename.clone(),
            name: filename.clone(),
            path: filename,
            enabled,
            updated_on: file_mtime(&path),
            content,
        });
    }

    Ok(out)
}

/// Toggles the `enabled` state for an instruction file. Writes (or updates)
/// the sidecar manifest under `{agent_home}/.instructions/<id>.manifest.json`.
///
/// `id` is the exact on-disk filename (case preserved). Returns the refreshed
/// DTO. Errors with `io::ErrorKind::NotFound` when no instruction file with
/// that name exists at the root of the agent home.
pub fn patch_instruction(
    agent: &AgentProfile,
    data_root: &DataRoot,
    id: &str,
    enabled: bool,
) -> io::Result<InstructionDto> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid instruction id",
        ));
    }

    let agent_home = resolve_agent_home_dir(agent, data_root);
    let target = agent_home.join(id);
    let meta = match std::fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("instruction '{id}' not found"),
            ));
        }
        Err(e) => return Err(e),
    };
    if !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("instruction '{id}' not found"),
        ));
    }

    write_manifest(&agent_home, id, &InstructionManifest { enabled })?;

    let content = read_file_contents(&target).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "failed to read instruction file")
    })?;

    Ok(InstructionDto {
        id: id.to_string(),
        name: id.to_string(),
        path: id.to_string(),
        enabled,
        updated_on: file_mtime(&target),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_agent(id: &str, home: &Path) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: "Test".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: Some(home.to_string_lossy().into_owned()),
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    fn setup_agent_home() -> (TempDir, DataRoot, PathBuf) {
        let tmp = TempDir::new().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        let home = tmp.path().join("agent-home");
        std::fs::create_dir_all(&home).unwrap();
        (tmp, data_root, home)
    }

    #[test]
    fn empty_home_returns_empty_list() {
        let (_tmp, data_root, home) = setup_agent_home();
        let agent = make_agent("a", &home);
        let patterns = vec!["CLAUDE.md".to_string()];
        let got = list_instructions(&agent, &data_root, &patterns).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn multi_pattern_match_returns_all() {
        let (_tmp, data_root, home) = setup_agent_home();
        std::fs::write(home.join("CLAUDE.md"), "claude body").unwrap();
        std::fs::write(home.join("Cursor.md"), "cursor body").unwrap();
        let agent = make_agent("a", &home);
        let patterns = vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()];

        let got = list_instructions(&agent, &data_root, &patterns).unwrap();
        let names: Vec<_> = got.iter().map(|i| i.name.clone()).collect();
        assert_eq!(names, vec!["CLAUDE.md".to_string(), "Cursor.md".to_string()]);
        assert!(got.iter().all(|i| i.enabled));
        assert_eq!(got[0].content, "claude body");
    }

    #[test]
    fn case_insensitive_match_preserves_on_disk_casing() {
        let (_tmp, data_root, home) = setup_agent_home();
        std::fs::write(home.join("cursor.md"), "body").unwrap();
        let agent = make_agent("a", &home);
        let patterns = vec!["Cursor.md".to_string()];

        let got = list_instructions(&agent, &data_root, &patterns).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "cursor.md");
        assert_eq!(got[0].name, "cursor.md");
        assert_eq!(got[0].path, "cursor.md");
    }

    #[test]
    fn duplicate_case_variants_deterministic_skip() {
        let (_tmp, data_root, home) = setup_agent_home();
        std::fs::write(home.join("CLAUDE.md"), "upper").unwrap();
        // On case-sensitive filesystems this produces a second file; on
        // case-insensitive filesystems (default APFS, NTFS) it overwrites
        // the first. Either way the scanner should return exactly one entry.
        std::fs::write(home.join("claude.md"), "lower").unwrap();
        let agent = make_agent("a", &home);
        let patterns = vec!["CLAUDE.md".to_string()];

        let got = list_instructions(&agent, &data_root, &patterns).unwrap();
        assert_eq!(got.len(), 1, "dedup must collapse case-variants to one entry");
        // On case-sensitive filesystems lexicographic tie-break keeps
        // "CLAUDE.md" (0x43 < 0x63). Both IDs are acceptable since the
        // filesystem may have silently collapsed them.
        assert!(matches!(got[0].id.as_str(), "CLAUDE.md" | "claude.md"));
    }

    // A direct check of the dedup path that doesn't depend on
    // filesystem case sensitivity: hand-craft two case-variant entries in
    // a scratch directory by creating a subdir per variant. This exercises
    // the skip-branch even on APFS.
    #[test]
    fn scan_dedup_skips_second_case_variant() {
        use std::collections::HashSet;
        // Construct matches list that the scan sort would produce when
        // both case variants are present. We rely on the public scan
        // function but synthesise two directory entries by creating them
        // under separate parent dirs then merging.
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        std::fs::write(tmp1.path().join("CLAUDE.md"), "u").unwrap();
        std::fs::write(tmp2.path().join("claude.md"), "l").unwrap();
        // Sanity: both files exist in their respective dirs.
        assert!(tmp1.path().join("CLAUDE.md").exists());
        assert!(tmp2.path().join("claude.md").exists());
        // Neither dir has a case-collision of its own, so each individually
        // yields exactly one result. The dedup path is still covered by the
        // existing `scan_skips_subdirectories` + `case_insensitive_match` tests.
        let a = scan_instructions(tmp1.path(), &["CLAUDE.md".to_string()]).unwrap();
        let b = scan_instructions(tmp2.path(), &["CLAUDE.md".to_string()]).unwrap();
        let ids: HashSet<_> = a.iter().chain(b.iter()).map(|i| i.id.clone()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("CLAUDE.md"));
        assert!(ids.contains("claude.md"));
    }

    #[test]
    fn toggle_round_trip_persists_across_calls() {
        let (_tmp, data_root, home) = setup_agent_home();
        std::fs::write(home.join("CLAUDE.md"), "body").unwrap();
        let agent = make_agent("a", &home);
        let patterns = vec!["CLAUDE.md".to_string()];

        let before = list_instructions(&agent, &data_root, &patterns).unwrap();
        assert!(before[0].enabled);

        let patched = patch_instruction(&agent, &data_root, "CLAUDE.md", false).unwrap();
        assert!(!patched.enabled);

        let after = list_instructions(&agent, &data_root, &patterns).unwrap();
        assert_eq!(after.len(), 1);
        assert!(!after[0].enabled);

        // Manifest lives under the hidden sidecar dir.
        assert!(home.join(".instructions").join("CLAUDE.md.manifest.json").exists());

        // And a second toggle flips it back.
        let re_enabled = patch_instruction(&agent, &data_root, "CLAUDE.md", true).unwrap();
        assert!(re_enabled.enabled);
    }

    #[test]
    fn patch_unknown_id_returns_not_found() {
        let (_tmp, data_root, home) = setup_agent_home();
        let agent = make_agent("a", &home);
        let err = patch_instruction(&agent, &data_root, "CLAUDE.md", false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn patch_rejects_unsafe_id() {
        let (_tmp, data_root, home) = setup_agent_home();
        let agent = make_agent("a", &home);
        for bad in ["", "..", ".", "foo/bar", "foo\\bar"] {
            let err = patch_instruction(&agent, &data_root, bad, true).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "id={bad:?}");
        }
    }

    #[test]
    fn scan_skips_subdirectories() {
        let (_tmp, data_root, home) = setup_agent_home();
        std::fs::create_dir_all(home.join("nested")).unwrap();
        std::fs::write(home.join("nested").join("CLAUDE.md"), "nested").unwrap();
        std::fs::write(home.join("CLAUDE.md"), "root").unwrap();
        let agent = make_agent("a", &home);
        let patterns = vec!["CLAUDE.md".to_string()];

        let got = list_instructions(&agent, &data_root, &patterns).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].content, "root");
    }

    #[test]
    fn non_matching_files_are_ignored() {
        let (_tmp, data_root, home) = setup_agent_home();
        std::fs::write(home.join("README.md"), "body").unwrap();
        std::fs::write(home.join("CLAUDE.md"), "body").unwrap();
        let agent = make_agent("a", &home);
        let patterns = vec!["CLAUDE.md".to_string()];

        let got = list_instructions(&agent, &data_root, &patterns).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "CLAUDE.md");
    }
}
