use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ao_persistence::paths::DataRoot;
use ao_protocol::agent::{canonical_project_key, AgentProfile};
pub use ao_protocol::rules::AddedBy;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Whether a skill comes from the user pool or a plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    #[default]
    User,
    Plugin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillDto {
    pub id: String,
    pub title: String,
    pub description: String,
    pub added_by: AddedBy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub auto_sync: bool,
    pub enabled: bool,
    pub updated_on: DateTime<Utc>,
    pub added_on: DateTime<Utc>,
    #[serde(default)]
    pub usage_count: u64,
    #[serde(default)]
    pub last_used: Option<DateTime<Utc>>,
    /// Whether this skill comes from the user pool or a plugin.
    #[serde(default)]
    pub source: SkillSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillManifest {
    pub added_by: AddedBy,
    pub enabled: bool,
    pub auto_sync: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub imported_at: DateTime<Utc>,
}

/// Returns the global user-pool skills directory: `<data_root>/skills/`.
pub fn resolve_user_pool_dir(data_root: &DataRoot) -> PathBuf {
    data_root.root().join("skills")
}

/// Creates the skills directory if it is missing.
pub fn ensure_agent_skills_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Scans the user pool for skills whose names appear in `agent_skills`, returning
/// one DTO per match. Skills not present in the pool are silently skipped.
/// Results are sorted by id.
pub fn scan_user_pool_for_agent(pool_dir: &Path, agent_skills: &[String]) -> Vec<SkillDto> {
    let mut skills = Vec::new();
    for skill_name in agent_skills {
        let folder = pool_dir.join(skill_name);
        if folder.is_dir() {
            let (_, ctime) = file_times(&folder);
            let manifest =
                read_manifest(&folder).unwrap_or_else(|| default_manifest_for_existing(ctime));
            let before = skills.len();
            walk_bundle_for_skills(pool_dir, &folder, &manifest, &mut skills);
            if skills.len() == before {
                if let Some(dto) = folder_skill_dto_fallback(pool_dir, &folder, &manifest) {
                    skills.push(dto);
                }
            }
            continue;
        }
        let flat = pool_dir.join(format!("{skill_name}.md"));
        if flat.is_file() {
            if let Some(dto) = flat_skill_to_dto(&flat) {
                skills.push(dto);
            }
        }
    }
    skills.sort_by(|a, b| a.id.cmp(&b.id));
    skills
}

/// Scans the plugin pool for skills enabled for `agent` and returns DTOs with
/// `source: SkillSource::Plugin`. Results are sorted by id.
pub fn scan_plugin_pool_for_agent(data_root_path: &Path, agent: &AgentProfile) -> Vec<SkillDto> {
    let mut skills = Vec::new();
    for (plugin_name, enablement) in &agent.enabled_plugins {
        if !enablement.enabled {
            continue;
        }
        let plugin_skills_dir = data_root_path
            .join("plugins")
            .join(plugin_name)
            .join("skills");
        if !plugin_skills_dir.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&plugin_skills_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let skill_name = match path.file_name() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => continue,
            };
            if let Some(allowed) = &enablement.enabled_skills {
                if !allowed.contains(&skill_name) {
                    continue;
                }
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let (title, description) = read_frontmatter_from(&skill_md);
            let (mtime, ctime) = file_times(&skill_md);
            skills.push(SkillDto {
                id: skill_name.clone(),
                title: title.unwrap_or_else(|| skill_name),
                description: description.unwrap_or_default(),
                added_by: AddedBy::Agent,
                source_url: None,
                auto_sync: false,
                enabled: true,
                updated_on: mtime,
                added_on: ctime,
                usage_count: 0,
                last_used: None,
                source: SkillSource::Plugin,
            });
        }
    }
    skills.sort_by(|a, b| a.id.cmp(&b.id));
    skills
}

/// One convention-folder skill discovered by scanning a `.launchpad/skills`
/// directory (global or project-scoped). Deliberately lighter than
/// [`SkillDto`]: convention-folder skills carry no trust stamps or usage
/// bookkeeping — they are a separate pool source, gated purely by
/// per-agent enablement (`AgentProfile::enabled_launchpad_global_skills` /
/// `enabled_launchpad_project_skills`), not by `trust_gate`/`SkillRegister`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchpadSkillEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: PathBuf,
}

/// Scans `skills_dir` for convention-folder skills: every subdirectory
/// containing a `SKILL.md` (the same marker `scan_plugin_pool_for_agent`
/// looks for) becomes an available entry. Returns an empty list when the
/// directory is absent or unreadable — this is a passive discovery scan and
/// never errors on a missing convention folder. Results are sorted by name.
fn scan_launchpad_skills_dir(skills_dir: &Path) -> Vec<LaunchpadSkillEntry> {
    let entries = match std::fs::read_dir(skills_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let name = match path.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => continue,
        };
        let (_, description) = read_frontmatter_from(&skill_md);
        skills.push(LaunchpadSkillEntry {
            name,
            description,
            path,
        });
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Scans the global convention-folder skills root, `<data_root>/.launchpad/skills`.
/// `data_root` must already be resolved by the caller (via
/// `ao_protocol::data_root::resolve_data_root`) — this function never
/// hardcodes a home-relative path, matching every other data-root consumer
/// in the codebase. Empty when the directory doesn't exist yet.
pub fn scan_launchpad_global_skills(data_root: &Path) -> Vec<LaunchpadSkillEntry> {
    scan_launchpad_skills_dir(&data_root.join(".launchpad").join("skills"))
}

/// Scans the project convention-folder skills root, `<focus_path>/.launchpad/skills`.
/// Empty when `focus_path` is `None` (no thread focus) or the directory
/// doesn't exist (the focused project hasn't adopted the convention) — both
/// are normal, not error, conditions.
pub fn scan_launchpad_project_skills(focus_path: Option<&str>) -> Vec<LaunchpadSkillEntry> {
    let Some(focus_path) = focus_path else {
        return Vec::new();
    };
    scan_launchpad_skills_dir(&Path::new(focus_path).join(".launchpad").join("skills"))
}

/// Resolves the effective set of convention-folder skills for one turn,
/// mirroring how `scan_plugin_pool_for_agent` filters the plugin pool by
/// per-agent enablement:
///
/// - Global entries are scanned via [`scan_launchpad_global_skills`] and
///   kept only if their name appears in `agent.enabled_launchpad_global_skills`
///   (absent/empty means none enabled — explicit opt-in, unlike the plugin
///   pool's `None = all`).
/// - Project entries are scanned via [`scan_launchpad_project_skills`] only
///   when `focus_path` is set, and kept only if their name appears in
///   `agent.enabled_launchpad_project_skills[canonical_project_key(focus_path)]`.
/// - Collision: per the locked decision, a project skill shadows
///   a global skill of the same name. The shadowed global entry is simply
///   omitted from the result — neither enablement record is mutated, so the
///   shadow disappears on its own once the project skill is disabled/removed.
pub fn resolve_effective_launchpad_skills(
    data_root: &Path,
    agent: &AgentProfile,
    focus_path: Option<&str>,
) -> Vec<LaunchpadSkillEntry> {
    let enabled_global = agent
        .enabled_launchpad_global_skills
        .as_deref()
        .unwrap_or(&[]);
    let mut effective: Vec<LaunchpadSkillEntry> = scan_launchpad_global_skills(data_root)
        .into_iter()
        .filter(|s| enabled_global.iter().any(|n| n == &s.name))
        .collect();

    let project: Vec<LaunchpadSkillEntry> = match focus_path {
        Some(fp) => {
            let key = canonical_project_key(fp);
            let enabled_project = agent
                .enabled_launchpad_project_skills
                .get(&key)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            scan_launchpad_project_skills(Some(fp))
                .into_iter()
                .filter(|s| enabled_project.iter().any(|n| n == &s.name))
                .collect()
        }
        None => Vec::new(),
    };

    // Project shadows global: drop any global entry whose name also won a
    // spot in the project set before merging the project entries in.
    effective.retain(|g| !project.iter().any(|p| p.name == g.name));
    effective.extend(project);
    effective.sort_by(|a, b| a.name.cmp(&b.name));
    effective
}

/// Outcome of [`promote_launchpad_skill_to_global`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteLaunchpadSkillOutcome {
    /// The project skill folder was copied to the global root.
    Promoted,
    /// A folder with this name already exists at the global root; nothing
    /// was copied or overwritten.
    AlreadyExistsGlobally,
}

/// Promotes a project convention-folder skill to the global root ("Make
/// available globally"): copies
/// `<focus_path>/.launchpad/skills/<skill_name>/` into
/// `<data_root>/.launchpad/skills/<skill_name>/`.
///
/// Name-collision policy is refuse-and-report: if a folder
/// already exists at the global destination, this returns
/// `AlreadyExistsGlobally` without touching it — it never overwrites an
/// existing global skill. Errors with `io::ErrorKind::NotFound` when the
/// source project skill folder doesn't exist.
pub fn promote_launchpad_skill_to_global(
    data_root: &Path,
    focus_path: &str,
    skill_name: &str,
) -> io::Result<PromoteLaunchpadSkillOutcome> {
    let src = Path::new(focus_path)
        .join(".launchpad")
        .join("skills")
        .join(skill_name);
    if !src.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "project skill '{skill_name}' not found at {}",
                src.display()
            ),
        ));
    }

    let dst = data_root.join(".launchpad").join("skills").join(skill_name);
    if dst.exists() {
        return Ok(PromoteLaunchpadSkillOutcome::AlreadyExistsGlobally);
    }

    copy_dir_recursive(&src, &dst)?;
    Ok(PromoteLaunchpadSkillOutcome::Promoted)
}

/// Copies `src` (a directory) into `pool_dir/<name>` and writes a top-level
/// `.manifest.json` with `added_by=user`. Returns the canonical name used (may
/// have `-from-<agent_id_short>` appended on collision) and the list of skills
/// discovered. `agent_id_short` is the first 8 chars of the agent id.
pub fn import_folder_to_pool(
    pool_dir: &Path,
    src: &Path,
    agent_id_short: &str,
) -> io::Result<(String, Vec<SkillDto>)> {
    let meta = std::fs::metadata(src)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "src_path must be a directory",
        ));
    }
    let src_name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid src_path"))?
        .to_string_lossy()
        .into_owned();

    ensure_agent_skills_dir(pool_dir)?;

    let canonical_name = if pool_dir.join(&src_name).exists() {
        format!("{src_name}-from-{agent_id_short}")
    } else {
        src_name
    };

    let target = pool_dir.join(&canonical_name);
    copy_dir_recursive(src, &target)?;

    let manifest = SkillManifest {
        added_by: AddedBy::User,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at: Utc::now(),
    };
    write_manifest(&target, &manifest)?;

    let dtos = scan_bundle(pool_dir, &target);
    Ok((canonical_name, dtos))
}

/// Copies `src` (a single `.md` file) into `pool_dir/<name>.md` and writes a
/// sibling manifest. Returns the canonical stem used and the SkillDto.
/// Uses `-from-<agent_id_short>` suffix on collision.
pub fn import_file_to_pool(
    pool_dir: &Path,
    src: &Path,
    agent_id_short: &str,
) -> io::Result<(String, SkillDto)> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if !ext.eq_ignore_ascii_case("md") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file must have a .md extension",
        ));
    }
    let meta = std::fs::metadata(src)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "src_path must be a file",
        ));
    }
    let stem = src
        .file_stem()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?
        .to_string_lossy()
        .into_owned();

    ensure_agent_skills_dir(pool_dir)?;

    let canonical_stem = if pool_dir.join(format!("{stem}.md")).exists() {
        format!("{stem}-from-{agent_id_short}")
    } else {
        stem
    };

    let target = pool_dir.join(format!("{canonical_stem}.md"));
    std::fs::copy(src, &target)?;

    let manifest = SkillManifest {
        added_by: AddedBy::User,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at: Utc::now(),
    };
    write_manifest(&target, &manifest)?;

    let dto = flat_skill_to_dto(&target).ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "failed to build SkillDto for imported file")
    })?;
    Ok((canonical_stem, dto))
}

/// Returns the sidecar manifest path for a skill. Flat `.md` skills get a
/// sibling `<stem>.manifest.json`; everything else is treated as a folder
/// skill and gets `<skill>/.manifest.json`.
fn manifest_path(skill_path: &Path) -> PathBuf {
    let is_flat_md = skill_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false);
    if is_flat_md {
        let parent = skill_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = skill_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        parent.join(format!("{stem}.manifest.json"))
    } else {
        skill_path.join(".manifest.json")
    }
}

pub fn read_manifest(skill_path: &Path) -> Option<SkillManifest> {
    let path = manifest_path(skill_path);
    let contents = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn write_manifest(skill_path: &Path, manifest: &SkillManifest) -> io::Result<()> {
    let path = manifest_path(skill_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)
}

fn system_time_to_utc(time: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(time)
}

fn file_times(path: &Path) -> (DateTime<Utc>, DateTime<Utc>) {
    let meta = std::fs::metadata(path).ok();
    let now = Utc::now();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(system_time_to_utc)
        .unwrap_or(now);
    let ctime = meta
        .as_ref()
        .and_then(|m| m.created().ok())
        .map(system_time_to_utc)
        .unwrap_or(mtime);
    (mtime, ctime)
}

/// Parses title and description from a Markdown file's YAML frontmatter.
/// Returns `(None, None)` when no frontmatter delimiter pair is present.
fn parse_skill_frontmatter(content: &str) -> (Option<String>, Option<String>) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, None);
    }
    let after_first = trimmed[3..].trim_start_matches(['\r', '\n']);
    let end = match after_first.find("\n---") {
        Some(pos) => pos,
        None => return (None, None),
    };
    let frontmatter = &after_first[..end];

    let mut title = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("title:") {
            title = Some(val.trim().trim_matches('"').to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().trim_matches('"').to_string());
        }
    }
    (title, description)
}

/// Reads `path` if it exists and returns parsed frontmatter.
fn read_frontmatter_from(path: &Path) -> (Option<String>, Option<String>) {
    match std::fs::read_to_string(path) {
        Ok(content) => parse_skill_frontmatter(&content),
        Err(_) => (None, None),
    }
}

fn default_manifest_for_existing(imported_at: DateTime<Utc>) -> SkillManifest {
    SkillManifest {
        added_by: AddedBy::Agent,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at,
    }
}

/// Locates the primary skill file inside `folder`: prefers `SKILL.md`, falls
/// back to `<folder_name>.md`. Returns `None` if neither exists.
fn find_skill_md_in(folder: &Path) -> Option<PathBuf> {
    let primary = folder.join("SKILL.md");
    if primary.is_file() {
        return Some(primary);
    }
    let folder_name = folder.file_name()?.to_string_lossy().into_owned();
    let alt = folder.join(format!("{folder_name}.md"));
    if alt.is_file() {
        return Some(alt);
    }
    None
}

/// Converts a path relative to the skills root into a forward-slash skill id.
fn rel_path_to_id(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether the bundle walker should descend into a directory. Skips hidden
/// folders and common dev artifacts that never contain user-facing skills.
fn should_descend_into(name: &str) -> bool {
    !(name.starts_with('.') || name == "node_modules" || name == "target")
}

/// Builds a DTO for a folder-skill under `skills_dir`. Inherited fields
/// (added_by, source_url, auto_sync, imported_at) come from `parent_manifest`
/// (the top-level bundle's manifest). The sub-folder's own `.manifest.json`
/// (if present) contributes the `enabled` override; otherwise it inherits.
fn folder_skill_dto(
    skills_dir: &Path,
    folder: &Path,
    skill_md: &Path,
    parent_manifest: &SkillManifest,
) -> Option<SkillDto> {
    let rel = folder.strip_prefix(skills_dir).ok()?;
    let id = rel_path_to_id(rel);
    if id.is_empty() {
        return None;
    }
    let folder_name = folder.file_name()?.to_string_lossy().into_owned();

    let (title, description) = read_frontmatter_from(skill_md);
    let (mtime, _) = file_times(skill_md);

    let own_manifest = read_manifest(folder);
    let enabled = own_manifest
        .as_ref()
        .map(|m| m.enabled)
        .unwrap_or(parent_manifest.enabled);

    Some(SkillDto {
        id,
        title: title.unwrap_or(folder_name),
        description: description.unwrap_or_default(),
        added_by: parent_manifest.added_by,
        source_url: parent_manifest.source_url.clone(),
        auto_sync: parent_manifest.auto_sync,
        enabled,
        updated_on: mtime,
        added_on: parent_manifest.imported_at,
        usage_count: 0,
        last_used: None,
        source: SkillSource::User,
    })
}

/// Builds a synthetic DTO for a top-level folder that has no SKILL.md
/// anywhere inside it. Used only as a fallback so explicitly-imported folders
/// remain visible in the UI until the user adds a SKILL.md.
fn folder_skill_dto_fallback(
    skills_dir: &Path,
    folder: &Path,
    manifest: &SkillManifest,
) -> Option<SkillDto> {
    let rel = folder.strip_prefix(skills_dir).ok()?;
    let id = rel_path_to_id(rel);
    if id.is_empty() {
        return None;
    }
    let folder_name = folder.file_name()?.to_string_lossy().into_owned();
    let (mtime, _) = file_times(folder);

    Some(SkillDto {
        id,
        title: folder_name,
        description: String::new(),
        added_by: manifest.added_by,
        source_url: manifest.source_url.clone(),
        auto_sync: manifest.auto_sync,
        enabled: manifest.enabled,
        updated_on: mtime,
        added_on: manifest.imported_at,
        usage_count: 0,
        last_used: None,
        source: SkillSource::User,
    })
}

fn flat_skill_to_dto(file: &Path) -> Option<SkillDto> {
    let id = file.file_stem()?.to_string_lossy().into_owned();
    let (title, description) = read_frontmatter_from(file);
    let (mtime, ctime) = file_times(file);
    let manifest = read_manifest(file).unwrap_or_else(|| default_manifest_for_existing(ctime));

    Some(SkillDto {
        id: id.clone(),
        title: title.unwrap_or_else(|| id.clone()),
        description: description.unwrap_or_default(),
        added_by: manifest.added_by,
        source_url: manifest.source_url,
        auto_sync: manifest.auto_sync,
        enabled: manifest.enabled,
        updated_on: mtime,
        added_on: manifest.imported_at,
        usage_count: 0,
        last_used: None,
        source: SkillSource::User,
    })
}

/// Recursively walks `folder`, emitting a DTO for every sub-folder that
/// directly contains a `SKILL.md` (or `<folder>.md`). Walking continues past
/// matches so bundles with both a root SKILL.md and nested ones yield every
/// skill discovered.
fn walk_bundle_for_skills(
    skills_dir: &Path,
    folder: &Path,
    parent_manifest: &SkillManifest,
    out: &mut Vec<SkillDto>,
) {
    if let Some(skill_md) = find_skill_md_in(folder) {
        if let Some(dto) = folder_skill_dto(skills_dir, folder, &skill_md, parent_manifest) {
            out.push(dto);
        }
    }

    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let child = entry.path();
        let Some(name) = child.file_name() else {
            continue;
        };
        let name_str = name.to_string_lossy();
        if !should_descend_into(&name_str) {
            continue;
        }
        walk_bundle_for_skills(skills_dir, &child, parent_manifest, out);
    }
}

/// Discovers every skill inside a single top-level bundle folder. Used by
/// import endpoints to return the list of skills surfaced by an import.
fn scan_bundle(skills_dir: &Path, bundle_folder: &Path) -> Vec<SkillDto> {
    let (_, ctime) = file_times(bundle_folder);
    let manifest =
        read_manifest(bundle_folder).unwrap_or_else(|| default_manifest_for_existing(ctime));
    let mut out = Vec::new();
    walk_bundle_for_skills(skills_dir, bundle_folder, &manifest, &mut out);
    if out.is_empty() {
        if let Some(dto) = folder_skill_dto_fallback(skills_dir, bundle_folder, &manifest) {
            out.push(dto);
        }
    }
    out
}

/// Returns a target folder name within `parent` that does not yet exist.
/// Appends `-2`, `-3`, ... when the base name collides.
fn unique_folder_name(parent: &Path, name: &str) -> String {
    if !parent.join(name).exists() {
        return name.to_string();
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{name}-{n}");
        if !parent.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Returns a target `<stem>.<ext>` filename within `parent` that does not yet
/// exist. Appends `-2`, `-3`, ... to the stem when the base collides.
fn unique_file_stem(parent: &Path, stem: &str, ext: &str) -> String {
    if !parent.join(format!("{stem}.{ext}")).exists() {
        return stem.to_string();
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{stem}-{n}");
        if !parent.join(format!("{candidate}.{ext}")).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Recursively copies the contents of `src` into `dst`, creating `dst` if it
/// does not exist. Symlinks are followed (copied as files/dirs).
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Copies `src` (a directory) into `agent_skills_dir/<unique-name>` and writes
/// a top-level `.manifest.json` with `added_by=user`. Returns the list of
/// skills discovered in the imported bundle (one per `SKILL.md` found, or a
/// fallback placeholder when the bundle has no SKILL.md anywhere).
///
/// Errors with `io::ErrorKind::InvalidInput` when `src` is missing, is not a
/// directory, or has no usable file name. Other IO failures propagate as-is.
pub fn import_folder_as_skill(
    agent_skills_dir: &Path,
    src: &Path,
) -> io::Result<Vec<SkillDto>> {
    let meta = std::fs::metadata(src)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "src_path must be a directory",
        ));
    }
    let src_name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid src_path"))?
        .to_string_lossy()
        .into_owned();

    ensure_agent_skills_dir(agent_skills_dir)?;

    let target_name = unique_folder_name(agent_skills_dir, &src_name);
    let target = agent_skills_dir.join(&target_name);
    copy_dir_recursive(src, &target)?;

    let manifest = SkillManifest {
        added_by: AddedBy::User,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at: Utc::now(),
    };
    write_manifest(&target, &manifest)?;

    Ok(scan_bundle(agent_skills_dir, &target))
}

/// Parses an HTTPS GitHub URL and returns the derived repo name (final path
/// segment, stripped of a trailing `.git`). Returns
/// `io::ErrorKind::InvalidInput` on any non-github.com / non-https URL or
/// when the path has no usable segment.
pub fn parse_github_repo_name(url: &str) -> io::Result<String> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("https://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URL must start with https://"))?;
    let (host, path) = rest
        .split_once('/')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URL must include a path"))?;
    let host_lower = host.to_ascii_lowercase();
    if host_lower != "github.com" && host_lower != "www.github.com" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URL host must be github.com",
        ));
    }
    let last = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URL path has no repo name",
        ));
    }
    Ok(name.to_string())
}

/// Copies `src` (a single `.md` file) into `agent_skills_dir/<unique-name>.md`
/// and writes a sibling `<unique-name>.manifest.json` with `added_by=user`.
///
/// Errors with `io::ErrorKind::InvalidInput` when `src` is missing, is not a
/// file, or does not have a `.md` extension.
pub fn import_file_as_skill(agent_skills_dir: &Path, src: &Path) -> io::Result<SkillDto> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if !ext.eq_ignore_ascii_case("md") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file must have a .md extension",
        ));
    }
    let meta = std::fs::metadata(src)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "src_path must be a file",
        ));
    }
    let stem = src
        .file_stem()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?
        .to_string_lossy()
        .into_owned();

    ensure_agent_skills_dir(agent_skills_dir)?;

    let target_stem = unique_file_stem(agent_skills_dir, &stem, "md");
    let target = agent_skills_dir.join(format!("{target_stem}.md"));
    std::fs::copy(src, &target)?;

    let manifest = SkillManifest {
        added_by: AddedBy::User,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at: Utc::now(),
    };
    write_manifest(&target, &manifest)?;

    flat_skill_to_dto(&target).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "failed to build SkillDto for imported file",
        )
    })
}

/// Rejects skill ids that are empty or contain unsafe components. Allows
/// forward slashes as nested-skill segment separators but forbids empty
/// segments, `.`, `..`, and backslashes.
fn validate_skill_id(id: &str) -> io::Result<()> {
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill id must not be empty",
        ));
    }
    if id.contains('\\') || id.starts_with('/') || id.ends_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "skill id contains invalid characters",
        ));
    }
    for segment in id.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "skill id contains invalid segments",
            ));
        }
    }
    Ok(())
}

enum SkillLocation {
    TopLevelFlat(PathBuf),
    TopLevelFolder {
        folder: PathBuf,
    },
    NestedFolder {
        sub_folder: PathBuf,
        top_folder: PathBuf,
    },
}

/// Resolves a skill id (possibly containing `/`) to its on-disk shape.
/// Returns `io::ErrorKind::NotFound` when no matching skill exists and
/// `io::ErrorKind::InvalidInput` when the id is unsafe.
fn locate_skill(skills_dir: &Path, skill_id: &str) -> io::Result<SkillLocation> {
    validate_skill_id(skill_id)?;

    if !skill_id.contains('/') {
        let folder = skills_dir.join(skill_id);
        if folder.is_dir() {
            return Ok(SkillLocation::TopLevelFolder { folder });
        }
        let flat = skills_dir.join(format!("{skill_id}.md"));
        if flat.is_file() {
            return Ok(SkillLocation::TopLevelFlat(flat));
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("skill '{skill_id}' not found"),
        ));
    }

    let top_segment = skill_id
        .split('/')
        .next()
        .expect("split always yields at least one segment");
    let top_folder = skills_dir.join(top_segment);
    if !top_folder.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("top-level skill '{top_segment}' not found"),
        ));
    }

    let mut sub_folder = skills_dir.to_path_buf();
    for segment in skill_id.split('/') {
        sub_folder.push(segment);
    }
    if !sub_folder.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("nested skill '{skill_id}' not found"),
        ));
    }
    if find_skill_md_in(&sub_folder).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("nested skill '{skill_id}' has no SKILL.md"),
        ));
    }

    Ok(SkillLocation::NestedFolder {
        sub_folder,
        top_folder,
    })
}

/// Removes a top-level skill by id. For folder skills the directory is
/// removed recursively; for flat `.md` skills the file and any sibling
/// sidecar manifest are removed. Nested skills cannot be deleted
/// individually — disable them or delete the whole bundle instead.
pub fn delete_skill(skills_dir: &Path, skill_id: &str) -> io::Result<()> {
    match locate_skill(skills_dir, skill_id)? {
        SkillLocation::TopLevelFlat(path) => {
            let sidecar = manifest_path(&path);
            std::fs::remove_file(&path)?;
            if sidecar.exists() {
                std::fs::remove_file(&sidecar)?;
            }
            Ok(())
        }
        SkillLocation::TopLevelFolder { folder } => {
            std::fs::remove_dir_all(&folder)?;
            Ok(())
        }
        SkillLocation::NestedFolder { .. } => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot delete a nested skill; disable it or delete the top-level bundle",
        )),
    }
}

/// Fields that may be updated on an existing skill's sidecar manifest.
/// Unset fields are preserved as-is on write.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkillPatch {
    pub enabled: Option<bool>,
    pub auto_sync: Option<bool>,
}

/// Applies `patch` to the skill identified by `skill_id`. Only the provided
/// fields are updated; untouched fields are preserved. Returns the fresh DTO.
///
/// Errors:
/// - `io::ErrorKind::NotFound` — skill does not exist.
/// - `io::ErrorKind::InvalidInput` — `auto_sync=true` on a non-github skill,
///   `auto_sync=true` on a nested skill (toggle it on the bundle instead),
///   or an unsafe `skill_id`.
pub fn patch_skill(
    skills_dir: &Path,
    skill_id: &str,
    patch: SkillPatch,
) -> io::Result<SkillDto> {
    match locate_skill(skills_dir, skill_id)? {
        SkillLocation::TopLevelFlat(path) => {
            let (_, ctime) = file_times(&path);
            let mut manifest =
                read_manifest(&path).unwrap_or_else(|| default_manifest_for_existing(ctime));

            if let Some(enabled) = patch.enabled {
                manifest.enabled = enabled;
            }
            if let Some(auto_sync) = patch.auto_sync {
                if auto_sync && manifest.added_by != AddedBy::Github {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "auto_sync can only be enabled for github-imported skills",
                    ));
                }
                manifest.auto_sync = auto_sync;
            }
            write_manifest(&path, &manifest)?;

            flat_skill_to_dto(&path).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "failed to build SkillDto after patch")
            })
        }
        SkillLocation::TopLevelFolder { folder } => {
            let (_, ctime) = file_times(&folder);
            let mut manifest =
                read_manifest(&folder).unwrap_or_else(|| default_manifest_for_existing(ctime));

            if let Some(enabled) = patch.enabled {
                manifest.enabled = enabled;
            }
            if let Some(auto_sync) = patch.auto_sync {
                if auto_sync && manifest.added_by != AddedBy::Github {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "auto_sync can only be enabled for github-imported skills",
                    ));
                }
                manifest.auto_sync = auto_sync;
            }
            write_manifest(&folder, &manifest)?;

            match find_skill_md_in(&folder) {
                Some(skill_md) => folder_skill_dto(skills_dir, &folder, &skill_md, &manifest),
                None => folder_skill_dto_fallback(skills_dir, &folder, &manifest),
            }
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "failed to build SkillDto after patch")
            })
        }
        SkillLocation::NestedFolder {
            sub_folder,
            top_folder,
        } => {
            if let Some(true) = patch.auto_sync {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "auto_sync can only be toggled on the top-level bundle",
                ));
            }
            let (_, parent_ctime) = file_times(&top_folder);
            let parent_manifest = read_manifest(&top_folder)
                .unwrap_or_else(|| default_manifest_for_existing(parent_ctime));

            // Sub-skill's own manifest overrides only `enabled`. Other fields
            // are kept aligned with the parent at write time so re-reads stay
            // consistent even if the nested manifest is read on its own.
            let mut own_manifest = read_manifest(&sub_folder).unwrap_or(SkillManifest {
                added_by: parent_manifest.added_by,
                enabled: parent_manifest.enabled,
                auto_sync: false,
                source_url: parent_manifest.source_url.clone(),
                imported_at: parent_manifest.imported_at,
            });

            if let Some(enabled) = patch.enabled {
                own_manifest.enabled = enabled;
            }
            write_manifest(&sub_folder, &own_manifest)?;

            let skill_md = find_skill_md_in(&sub_folder).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "SKILL.md missing after patch")
            })?;
            folder_skill_dto(skills_dir, &sub_folder, &skill_md, &parent_manifest).ok_or_else(
                || io::Error::new(io::ErrorKind::Other, "failed to build SkillDto after patch"),
            )
        }
    }
}

/// Runs `git pull` against every auto-sync github bundle at the top level of
/// `skills_dir`, logging per-skill failures without aborting. Nested skills
/// inherit their parent's auto_sync flag but the pull always runs at the
/// bundle root. Returns the freshly scanned list after pull attempts. Safe
/// to call when the directory is missing (returns an empty vec).
pub fn refresh_agent_skills(skills_dir: &Path, agent_id: &str) -> Vec<SkillDto> {
    let initial = scan_skills_dir(skills_dir);
    for skill in &initial {
        if !(skill.auto_sync && skill.added_by == AddedBy::Github) {
            continue;
        }
        if skill.id.contains('/') {
            // Nested skill inherits auto_sync from its bundle; the bundle
            // root is pulled once (see the top-level entry) — skip here to
            // avoid redundant git invocations.
            continue;
        }
        let skill_path = skills_dir.join(&skill.id);
        if !skill_path.is_dir() {
            continue;
        }
        match std::process::Command::new("git")
            .arg("pull")
            .current_dir(&skill_path)
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!(
                    agent_id = %agent_id,
                    skill_id = %skill.id,
                    "git pull succeeded for auto-sync skill",
                );
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let tail: String = stderr
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n");
                tracing::warn!(
                    agent_id = %agent_id,
                    skill_id = %skill.id,
                    stderr = %tail.trim(),
                    "git pull failed for auto-sync skill",
                );
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    skill_id = %skill.id,
                    error = %e,
                    "git pull could not be invoked for auto-sync skill",
                );
            }
        }
    }
    scan_skills_dir(skills_dir)
}

/// Scans `skills_dir` for skills. Top-level flat `.md` files map to a single
/// skill; every top-level subdirectory is treated as a bundle and walked
/// recursively — each folder that directly contains a `SKILL.md` (or
/// `<folder>.md`) yields one DTO. Bundles with no SKILL.md anywhere fall
/// back to a single folder-name tile so explicitly-imported folders remain
/// visible. Results are sorted by id for deterministic ordering.
pub fn scan_skills_dir(skills_dir: &Path) -> Vec<SkillDto> {
    let entries = match std::fs::read_dir(skills_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            let (_, ctime) = file_times(&path);
            let manifest =
                read_manifest(&path).unwrap_or_else(|| default_manifest_for_existing(ctime));
            let before = skills.len();
            walk_bundle_for_skills(skills_dir, &path, &manifest, &mut skills);
            if skills.len() == before {
                // No SKILL.md anywhere in this bundle — emit a fallback tile
                // so the imported folder is still discoverable.
                if let Some(dto) = folder_skill_dto_fallback(skills_dir, &path, &manifest) {
                    skills.push(dto);
                }
            }
        } else if file_type.is_file() {
            let is_md = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if is_md {
                if let Some(dto) = flat_skill_to_dto(&path) {
                    skills.push(dto);
                }
            }
        }
    }

    skills.sort_by(|a, b| a.id.cmp(&b.id));
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_user_pool_dir_returns_skills_subdir_of_data_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let pool = resolve_user_pool_dir(&data_root);
        assert_eq!(pool, tmp.path().join("skills"));
    }

    #[test]
    fn ensure_agent_skills_dir_creates_missing_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("agent_homes").join("a").join("skills");
        assert!(!target.exists());

        ensure_agent_skills_dir(&target).unwrap();

        assert!(target.is_dir());
    }

    fn sample_manifest(added_by: AddedBy) -> SkillManifest {
        SkillManifest {
            added_by,
            enabled: true,
            auto_sync: false,
            source_url: None,
            imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn manifest_round_trip_for_folder_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("my-folder-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let manifest = sample_manifest(AddedBy::User);

        write_manifest(&skill_dir, &manifest).unwrap();

        assert!(skill_dir.join(".manifest.json").exists());
        let loaded = read_manifest(&skill_dir).expect("manifest present");
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn manifest_round_trip_for_flat_md_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills_dir = tmp.path();
        let skill_file = skills_dir.join("quick-tip.md");
        std::fs::write(&skill_file, "# Quick tip\n").unwrap();
        let manifest = SkillManifest {
            source_url: Some("https://github.com/owner/repo".to_string()),
            auto_sync: true,
            ..sample_manifest(AddedBy::Github)
        };

        write_manifest(&skill_file, &manifest).unwrap();

        let sidecar = skills_dir.join("quick-tip.manifest.json");
        assert!(sidecar.exists());
        let loaded = read_manifest(&skill_file).expect("manifest present");
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn read_manifest_returns_none_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("no-manifest");
        std::fs::create_dir_all(&skill_dir).unwrap();

        assert!(read_manifest(&skill_dir).is_none());
    }

    #[test]
    fn added_by_serializes_as_lowercase() {
        let json = serde_json::to_string(&AddedBy::Github).unwrap();
        assert_eq!(json, "\"github\"");
        let parsed: AddedBy = serde_json::from_str("\"user\"").unwrap();
        assert_eq!(parsed, AddedBy::User);
    }

    #[test]
    fn scan_missing_directory_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(scan_skills_dir(&missing).is_empty());
    }

    #[test]
    fn scan_empty_directory_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(scan_skills_dir(tmp.path()).is_empty());
    }

    #[test]
    fn scan_folder_and_flat_skill_defaults_without_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();

        // Folder skill with SKILL.md frontmatter.
        let folder = skills.join("my-folder");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\ntitle: \"Folder Title\"\ndescription: \"Folder desc\"\n---\n\nbody",
        )
        .unwrap();

        // Flat skill with frontmatter.
        std::fs::write(
            skills.join("quick-tip.md"),
            "---\ntitle: \"Quick Tip\"\ndescription: \"A short tip\"\n---\n\n# Tip",
        )
        .unwrap();

        let mut dtos = scan_skills_dir(skills);
        dtos.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(dtos.len(), 2);

        let folder_dto = &dtos[0];
        assert_eq!(folder_dto.id, "my-folder");
        assert_eq!(folder_dto.title, "Folder Title");
        assert_eq!(folder_dto.description, "Folder desc");
        assert_eq!(folder_dto.added_by, AddedBy::Agent);
        assert!(folder_dto.enabled);
        assert!(!folder_dto.auto_sync);
        assert!(folder_dto.source_url.is_none());

        let flat_dto = &dtos[1];
        assert_eq!(flat_dto.id, "quick-tip");
        assert_eq!(flat_dto.title, "Quick Tip");
        assert_eq!(flat_dto.description, "A short tip");
        assert_eq!(flat_dto.added_by, AddedBy::Agent);
        assert!(flat_dto.enabled);
        assert!(!flat_dto.auto_sync);
    }

    #[test]
    fn scan_honours_sidecar_manifest_for_flat_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();

        let skill = skills.join("github-pack.md");
        std::fs::write(&skill, "# no frontmatter").unwrap();
        let manifest = SkillManifest {
            added_by: AddedBy::Github,
            enabled: false,
            auto_sync: true,
            source_url: Some("https://github.com/owner/repo".to_string()),
            imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        write_manifest(&skill, &manifest).unwrap();

        let dtos = scan_skills_dir(skills);
        assert_eq!(dtos.len(), 1);
        let dto = &dtos[0];
        assert_eq!(dto.id, "github-pack");
        assert_eq!(dto.added_by, AddedBy::Github);
        assert!(!dto.enabled);
        assert!(dto.auto_sync);
        assert_eq!(dto.source_url.as_deref(), Some("https://github.com/owner/repo"));
        assert_eq!(dto.added_on, manifest.imported_at);
    }

    #[test]
    fn scan_falls_back_to_folder_name_when_no_skill_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let folder = skills.join("no-meta");
        std::fs::create_dir_all(&folder).unwrap();

        let dtos = scan_skills_dir(skills);
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "no-meta");
        assert_eq!(dtos[0].title, "no-meta");
        assert_eq!(dtos[0].description, "");
    }

    #[test]
    fn scan_recursively_finds_nested_skills_in_bundle() {
        // Mirrors the karpathy-skills layout: a top-level folder with no root
        // SKILL.md but nested `skills/<name>/SKILL.md` files.
        let tmp = tempfile::TempDir::new().unwrap();
        let skills_dir = tmp.path();

        let bundle = skills_dir.join("andrej-karpathy-skills");
        let nested_a = bundle.join("skills").join("guideline-one");
        let nested_b = bundle.join("skills").join("guideline-two");
        std::fs::create_dir_all(&nested_a).unwrap();
        std::fs::create_dir_all(&nested_b).unwrap();
        std::fs::write(
            nested_a.join("SKILL.md"),
            "---\ntitle: \"Guideline One\"\ndescription: \"First guideline\"\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            nested_b.join("SKILL.md"),
            "---\ntitle: \"Guideline Two\"\ndescription: \"Second guideline\"\n---\nbody",
        )
        .unwrap();
        write_manifest(
            &bundle,
            &SkillManifest {
                added_by: AddedBy::Github,
                enabled: true,
                auto_sync: true,
                source_url: Some("https://github.com/owner/karpathy".to_string()),
                imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            },
        )
        .unwrap();

        let mut dtos = scan_skills_dir(skills_dir);
        dtos.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(dtos.len(), 2, "expected two nested skills, got: {:?}", dtos);
        assert_eq!(dtos[0].id, "andrej-karpathy-skills/skills/guideline-one");
        assert_eq!(dtos[0].title, "Guideline One");
        assert_eq!(dtos[0].added_by, AddedBy::Github);
        assert!(dtos[0].auto_sync, "nested inherits auto_sync from bundle");
        assert_eq!(
            dtos[0].source_url.as_deref(),
            Some("https://github.com/owner/karpathy"),
        );
        assert_eq!(dtos[1].id, "andrej-karpathy-skills/skills/guideline-two");
    }

    #[test]
    fn scan_emits_both_root_and_nested_skills_when_both_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills_dir = tmp.path();
        let bundle = skills_dir.join("pack");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(
            bundle.join("SKILL.md"),
            "---\ntitle: \"Pack Root\"\ndescription: \"root skill\"\n---\n",
        )
        .unwrap();
        let nested = bundle.join("skills").join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\ntitle: \"Sub\"\ndescription: \"nested skill\"\n---\n",
        )
        .unwrap();

        let mut dtos = scan_skills_dir(skills_dir);
        dtos.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(dtos.len(), 2);
        assert_eq!(dtos[0].id, "pack");
        assert_eq!(dtos[0].title, "Pack Root");
        assert_eq!(dtos[1].id, "pack/skills/sub");
        assert_eq!(dtos[1].title, "Sub");
    }

    #[test]
    fn scan_skips_dot_git_when_walking_bundle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills_dir = tmp.path();
        let bundle = skills_dir.join("pack");
        // Place a SKILL.md inside a `.git` directory — it must be ignored.
        let git_dir = bundle.join(".git").join("accidental-skill");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("SKILL.md"), "---\ntitle: \"Oops\"\n---\n").unwrap();
        // And a real nested skill so the bundle isn't empty.
        let real = bundle.join("skills").join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("SKILL.md"), "---\ntitle: \"Real\"\n---\n").unwrap();

        let dtos = scan_skills_dir(skills_dir);
        assert!(dtos.iter().all(|d| !d.id.contains(".git")));
        assert!(dtos.iter().any(|d| d.id == "pack/skills/real"));
    }

    #[test]
    fn unique_folder_name_appends_numeric_suffix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path();

        assert_eq!(unique_folder_name(parent, "demo"), "demo");

        std::fs::create_dir_all(parent.join("demo")).unwrap();
        assert_eq!(unique_folder_name(parent, "demo"), "demo-2");

        std::fs::create_dir_all(parent.join("demo-2")).unwrap();
        assert_eq!(unique_folder_name(parent, "demo"), "demo-3");
    }

    #[test]
    fn unique_file_stem_appends_numeric_suffix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path();

        assert_eq!(unique_file_stem(parent, "tip", "md"), "tip");

        std::fs::write(parent.join("tip.md"), "first").unwrap();
        assert_eq!(unique_file_stem(parent, "tip", "md"), "tip-2");

        std::fs::write(parent.join("tip-2.md"), "second").unwrap();
        assert_eq!(unique_file_stem(parent, "tip", "md"), "tip-3");
    }

    #[test]
    fn import_file_rejects_non_md_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("notes.txt");
        std::fs::write(&src, "hello").unwrap();
        let skills_dir = tmp.path().join("skills");

        let err = import_file_as_skill(&skills_dir, &src).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!skills_dir.exists() || std::fs::read_dir(&skills_dir).unwrap().next().is_none());
    }

    #[test]
    fn import_file_writes_manifest_with_added_by_user() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("tip.md");
        std::fs::write(&src, "---\ntitle: \"Tip\"\ndescription: \"A tip\"\n---\nbody").unwrap();
        let skills_dir = tmp.path().join("skills");

        let dto = import_file_as_skill(&skills_dir, &src).unwrap();

        assert_eq!(dto.id, "tip");
        assert_eq!(dto.added_by, AddedBy::User);
        assert!(dto.enabled);
        assert!(!dto.auto_sync);
        assert_eq!(dto.title, "Tip");

        assert!(skills_dir.join("tip.md").exists());
        let manifest = read_manifest(&skills_dir.join("tip.md")).expect("manifest present");
        assert_eq!(manifest.added_by, AddedBy::User);
    }

    #[test]
    fn import_file_collision_appends_suffix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("tip.md");
        std::fs::write(&src, "# tip").unwrap();
        let skills_dir = tmp.path().join("skills");

        let first = import_file_as_skill(&skills_dir, &src).unwrap();
        let second = import_file_as_skill(&skills_dir, &src).unwrap();

        assert_eq!(first.id, "tip");
        assert_eq!(second.id, "tip-2");
        assert!(skills_dir.join("tip.md").exists());
        assert!(skills_dir.join("tip-2.md").exists());
    }

    #[test]
    fn import_folder_returns_nested_skills_when_no_root_skill_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("bundle");
        let nested = src.join("skills").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\ntitle: \"Inner\"\ndescription: \"nested\"\n---\n",
        )
        .unwrap();
        let skills_dir = tmp.path().join("skills");

        let skills = import_folder_as_skill(&skills_dir, &src).unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "bundle/skills/inner");
        assert_eq!(skills[0].title, "Inner");
        assert_eq!(skills[0].added_by, AddedBy::User);
    }

    #[test]
    fn import_folder_collision_appends_suffix_and_copies_contents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("pack");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("SKILL.md"), "# pack").unwrap();
        std::fs::write(src.join("nested").join("a.txt"), "A").unwrap();
        let skills_dir = tmp.path().join("skills");

        let first = import_folder_as_skill(&skills_dir, &src).unwrap();
        let second = import_folder_as_skill(&skills_dir, &src).unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "pack");
        assert_eq!(first[0].added_by, AddedBy::User);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, "pack-2");
        assert_eq!(second[0].added_by, AddedBy::User);

        assert!(skills_dir.join("pack").join("SKILL.md").exists());
        assert!(
            skills_dir
                .join("pack")
                .join("nested")
                .join("a.txt")
                .exists()
        );
        assert!(skills_dir.join("pack-2").join("SKILL.md").exists());
        assert!(skills_dir.join("pack").join(".manifest.json").exists());
        assert!(skills_dir.join("pack-2").join(".manifest.json").exists());
    }

    #[test]
    fn parse_github_repo_name_strips_dot_git_suffix() {
        assert_eq!(
            parse_github_repo_name("https://github.com/owner/repo.git").unwrap(),
            "repo",
        );
    }

    #[test]
    fn parse_github_repo_name_accepts_plain_url() {
        assert_eq!(
            parse_github_repo_name("https://github.com/owner/repo").unwrap(),
            "repo",
        );
    }

    #[test]
    fn parse_github_repo_name_handles_trailing_slash() {
        assert_eq!(
            parse_github_repo_name("https://github.com/owner/repo/").unwrap(),
            "repo",
        );
    }

    #[test]
    fn parse_github_repo_name_is_case_insensitive_on_host() {
        assert_eq!(
            parse_github_repo_name("https://GitHub.com/owner/repo").unwrap(),
            "repo",
        );
    }

    #[test]
    fn parse_github_repo_name_rejects_non_https() {
        let err = parse_github_repo_name("http://github.com/owner/repo").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn parse_github_repo_name_rejects_non_github_host() {
        let err = parse_github_repo_name("https://gitlab.com/owner/repo").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn parse_github_repo_name_rejects_bare_host() {
        let err = parse_github_repo_name("https://github.com").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn validate_skill_id_rejects_empty_and_traversal() {
        assert_eq!(
            validate_skill_id("").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            validate_skill_id("..").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            validate_skill_id("../escape").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            validate_skill_id("foo/../bar").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            validate_skill_id("foo//bar").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            validate_skill_id("/foo").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            validate_skill_id("foo/").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            validate_skill_id("foo\\bar").unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
        assert!(validate_skill_id("my-skill").is_ok());
        assert!(validate_skill_id("skill_1.2").is_ok());
        assert!(validate_skill_id("parent/child").is_ok());
        assert!(validate_skill_id("parent/skills/child").is_ok());
    }

    #[test]
    fn delete_skill_removes_folder_and_contents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let folder = skills.join("pack");
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        std::fs::write(folder.join("SKILL.md"), "# pack").unwrap();
        std::fs::write(folder.join("nested").join("a.txt"), "A").unwrap();
        write_manifest(&folder, &sample_manifest(AddedBy::User)).unwrap();

        delete_skill(skills, "pack").unwrap();

        assert!(!folder.exists());
    }

    #[test]
    fn delete_skill_removes_flat_file_and_sidecar() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let file = skills.join("tip.md");
        std::fs::write(&file, "# tip").unwrap();
        write_manifest(&file, &sample_manifest(AddedBy::User)).unwrap();
        let sidecar = skills.join("tip.manifest.json");
        assert!(sidecar.exists());

        delete_skill(skills, "tip").unwrap();

        assert!(!file.exists());
        assert!(!sidecar.exists());
    }

    #[test]
    fn delete_skill_returns_not_found_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = delete_skill(tmp.path(), "missing").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn delete_skill_rejects_traversal_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = delete_skill(tmp.path(), "../escape").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn delete_skill_rejects_nested_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let bundle = skills.join("bundle");
        let nested = bundle.join("skills").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("SKILL.md"), "---\ntitle: \"X\"\n---\n").unwrap();
        write_manifest(&bundle, &sample_manifest(AddedBy::Github)).unwrap();

        let err = delete_skill(skills, "bundle/skills/inner").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // The nested skill must still exist after the rejected delete.
        assert!(nested.join("SKILL.md").is_file());
    }

    #[test]
    fn patch_skill_updates_enabled_and_preserves_other_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let file = skills.join("tip.md");
        std::fs::write(&file, "# tip").unwrap();
        let initial = SkillManifest {
            added_by: AddedBy::Github,
            enabled: true,
            auto_sync: true,
            source_url: Some("https://github.com/owner/repo".to_string()),
            imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        write_manifest(&file, &initial).unwrap();

        let dto = patch_skill(
            skills,
            "tip",
            SkillPatch {
                enabled: Some(false),
                auto_sync: None,
            },
        )
        .unwrap();

        assert_eq!(dto.id, "tip");
        assert!(!dto.enabled);
        assert!(dto.auto_sync, "auto_sync must be preserved when not in patch");
        assert_eq!(dto.added_by, AddedBy::Github);
        assert_eq!(
            dto.source_url.as_deref(),
            Some("https://github.com/owner/repo"),
        );

        let reloaded = read_manifest(&file).unwrap();
        assert!(!reloaded.enabled);
        assert!(reloaded.auto_sync);
        assert_eq!(reloaded.added_by, AddedBy::Github);
        assert_eq!(
            reloaded.source_url.as_deref(),
            Some("https://github.com/owner/repo"),
        );
    }

    #[test]
    fn patch_skill_auto_sync_on_user_skill_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let folder = skills.join("pack");
        std::fs::create_dir_all(&folder).unwrap();
        write_manifest(&folder, &sample_manifest(AddedBy::User)).unwrap();

        let err = patch_skill(
            skills,
            "pack",
            SkillPatch {
                enabled: None,
                auto_sync: Some(true),
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        // Manifest must remain unchanged on rejection.
        let reloaded = read_manifest(&folder).unwrap();
        assert!(!reloaded.auto_sync);
        assert_eq!(reloaded.added_by, AddedBy::User);
    }

    #[test]
    fn patch_skill_returns_not_found_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = patch_skill(
            tmp.path(),
            "missing",
            SkillPatch {
                enabled: Some(true),
                auto_sync: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn patch_skill_auto_sync_false_on_user_skill_is_allowed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let folder = skills.join("pack");
        std::fs::create_dir_all(&folder).unwrap();
        write_manifest(&folder, &sample_manifest(AddedBy::User)).unwrap();

        let dto = patch_skill(
            skills,
            "pack",
            SkillPatch {
                enabled: None,
                auto_sync: Some(false),
            },
        )
        .unwrap();

        assert!(!dto.auto_sync);
        assert_eq!(dto.added_by, AddedBy::User);
    }

    #[test]
    fn patch_skill_nested_updates_enabled_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let bundle = skills.join("bundle");
        let nested = bundle.join("skills").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\ntitle: \"Inner\"\ndescription: \"d\"\n---\n",
        )
        .unwrap();
        write_manifest(
            &bundle,
            &SkillManifest {
                added_by: AddedBy::Github,
                enabled: true,
                auto_sync: true,
                source_url: Some("https://github.com/owner/repo".to_string()),
                imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            },
        )
        .unwrap();

        let dto = patch_skill(
            skills,
            "bundle/skills/inner",
            SkillPatch {
                enabled: Some(false),
                auto_sync: None,
            },
        )
        .unwrap();

        assert_eq!(dto.id, "bundle/skills/inner");
        assert!(!dto.enabled, "enabled override applies");
        // Inherited fields still come from the bundle manifest.
        assert_eq!(dto.added_by, AddedBy::Github);
        assert!(dto.auto_sync);
        assert_eq!(
            dto.source_url.as_deref(),
            Some("https://github.com/owner/repo"),
        );

        // Sub-folder gets its own sidecar.
        let sub_manifest = read_manifest(&nested).expect("sub-folder manifest");
        assert!(!sub_manifest.enabled);

        // Parent manifest must not be mutated by a nested patch.
        let parent_manifest = read_manifest(&bundle).unwrap();
        assert!(parent_manifest.enabled);
        assert!(parent_manifest.auto_sync);
    }

    #[test]
    fn patch_skill_nested_auto_sync_true_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();
        let bundle = skills.join("bundle");
        let nested = bundle.join("skills").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("SKILL.md"), "---\ntitle: \"I\"\n---\n").unwrap();
        write_manifest(&bundle, &sample_manifest(AddedBy::Github)).unwrap();

        let err = patch_skill(
            skills,
            "bundle/skills/inner",
            SkillPatch {
                enabled: None,
                auto_sync: Some(true),
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn import_folder_rejects_missing_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("does-not-exist");
        let skills_dir = tmp.path().join("skills");

        let err = import_folder_as_skill(&skills_dir, &src).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn refresh_agent_skills_returns_empty_for_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let out = refresh_agent_skills(&missing, "agent-a");
        assert!(out.is_empty());
    }

    #[test]
    fn refresh_agent_skills_returns_non_github_skill_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();

        // A user-imported flat skill with no auto_sync — must not be touched.
        let file = skills.join("tip.md");
        std::fs::write(&file, "---\ntitle: \"Tip\"\ndescription: \"A tip\"\n---\nbody").unwrap();
        write_manifest(&file, &sample_manifest(AddedBy::User)).unwrap();

        let out = refresh_agent_skills(skills, "agent-a");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "tip");
        assert_eq!(out[0].added_by, AddedBy::User);
        assert!(!out[0].auto_sync);
    }

    #[test]
    fn refresh_agent_skills_succeeds_when_no_auto_sync_skills() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skills = tmp.path();

        // Github-imported folder skill but auto_sync=false — must be returned
        // without triggering a git pull.
        let folder = skills.join("pack");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), "# pack").unwrap();
        let manifest = SkillManifest {
            added_by: AddedBy::Github,
            enabled: true,
            auto_sync: false,
            source_url: Some("https://github.com/owner/repo".to_string()),
            imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        };
        write_manifest(&folder, &manifest).unwrap();

        let out = refresh_agent_skills(skills, "agent-a");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "pack");
        assert_eq!(out[0].added_by, AddedBy::Github);
        assert!(!out[0].auto_sync);
    }

    fn write_launchpad_skill(root: &Path, name: &str, description: Option<&str>) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let body = match description {
            Some(desc) => format!(
                "---\ntitle: \"{name}\"\ndescription: \"{desc}\"\n---\nbody"
            ),
            None => "# no frontmatter".to_string(),
        };
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    fn make_launchpad_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: String::new(),
            emoji: None,
            provider: ao_protocol::agent::ProviderConfig::Cli(
                ao_protocol::agent::CliProviderConfig {
                    command: "echo".to_string(),
                    args: vec![],
                    normalizer: None,
                    output_format: ao_protocol::agent::OutputFormat::Text,
                    input_mode: ao_protocol::agent::InputMode::Arg,
                    model_arg: None,
                    model_aliases: std::collections::HashMap::new(),
                    system_prompt_arg: None,
                    session_arg: None,
                    resume_args: vec![],
                    session_id_fields: vec![],
                    clear_env: false,
                    no_output_timeout_ms: 30_000,
                    file_capabilities: None,
                },
            ),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: std::collections::HashMap::new(),
            max_instances: 1,
            timeout_seconds: 300,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            enabled_plugins: std::collections::HashMap::new(),
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

    #[test]
    fn scan_launchpad_global_skills_finds_marker_file_subdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let global_dir = tmp.path().join(".launchpad").join("skills");
        write_launchpad_skill(&global_dir, "commit-style", Some("How we write commits"));

        let found = scan_launchpad_global_skills(tmp.path());

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "commit-style");
        assert_eq!(found[0].description.as_deref(), Some("How we write commits"));
        assert_eq!(found[0].path, global_dir.join("commit-style"));
    }

    #[test]
    fn scan_launchpad_global_skills_empty_when_dir_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(scan_launchpad_global_skills(tmp.path()).is_empty());
    }

    #[test]
    fn scan_launchpad_project_skills_empty_when_focus_path_none() {
        assert!(scan_launchpad_project_skills(None).is_empty());
    }

    #[test]
    fn scan_launchpad_project_skills_empty_when_dir_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let focus = tmp.path().to_string_lossy().into_owned();
        assert!(scan_launchpad_project_skills(Some(&focus)).is_empty());
    }

    #[test]
    fn scan_launchpad_project_skills_finds_marker_file_subdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project_skills_dir = tmp.path().join(".launchpad").join("skills");
        write_launchpad_skill(&project_skills_dir, "repo-conventions", None);
        let focus = tmp.path().to_string_lossy().into_owned();

        let found = scan_launchpad_project_skills(Some(&focus));

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "repo-conventions");
        assert_eq!(found[0].description, None);
    }

    #[test]
    fn resolve_effective_launchpad_skills_only_includes_enabled_global_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let global_dir = tmp.path().join(".launchpad").join("skills");
        write_launchpad_skill(&global_dir, "enabled-one", None);
        write_launchpad_skill(&global_dir, "disabled-one", None);

        let mut agent = make_launchpad_agent("a");
        agent.enabled_launchpad_global_skills = Some(vec!["enabled-one".to_string()]);

        let effective = resolve_effective_launchpad_skills(tmp.path(), &agent, None);

        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].name, "enabled-one");
    }

    #[test]
    fn resolve_effective_launchpad_skills_excludes_all_global_when_none_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let global_dir = tmp.path().join(".launchpad").join("skills");
        write_launchpad_skill(&global_dir, "some-skill", None);

        // enabled_launchpad_global_skills left at its default (None) — explicit
        // opt-in means "none enabled", not "all enabled" (unlike the plugin pool).
        let agent = make_launchpad_agent("a");

        let effective = resolve_effective_launchpad_skills(tmp.path(), &agent, None);

        assert!(effective.is_empty());
    }

    #[test]
    fn resolve_effective_launchpad_skills_project_shadows_global_on_name_collision() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_root = tmp.path().join("data_root");
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&data_root).unwrap();
        std::fs::create_dir_all(&project_dir).unwrap();

        write_launchpad_skill(
            &data_root.join(".launchpad").join("skills"),
            "shared-name",
            Some("global version"),
        );
        write_launchpad_skill(
            &project_dir.join(".launchpad").join("skills"),
            "shared-name",
            Some("project version"),
        );
        // A global-only skill, to confirm non-colliding entries survive the merge.
        write_launchpad_skill(
            &data_root.join(".launchpad").join("skills"),
            "global-only",
            None,
        );

        let focus = project_dir.to_string_lossy().into_owned();
        let key = canonical_project_key(&focus);

        let mut agent = make_launchpad_agent("a");
        agent.enabled_launchpad_global_skills =
            Some(vec!["shared-name".to_string(), "global-only".to_string()]);
        agent
            .enabled_launchpad_project_skills
            .insert(key, vec!["shared-name".to_string()]);

        let effective = resolve_effective_launchpad_skills(&data_root, &agent, Some(&focus));

        assert_eq!(effective.len(), 2);
        let shared = effective
            .iter()
            .find(|s| s.name == "shared-name")
            .expect("shared-name present exactly once");
        assert_eq!(shared.description.as_deref(), Some("project version"));
        assert!(effective.iter().any(|s| s.name == "global-only"));

        // Both enablement records survive the collision untouched.
        assert_eq!(
            agent.enabled_launchpad_global_skills,
            Some(vec!["shared-name".to_string(), "global-only".to_string()])
        );
        assert_eq!(
            agent.enabled_launchpad_project_skills.get(&canonical_project_key(&focus)),
            Some(&vec!["shared-name".to_string()])
        );
    }

    #[test]
    fn resolve_effective_launchpad_skills_excludes_disabled_project_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project_dir = tmp.path().join("project");
        write_launchpad_skill(
            &project_dir.join(".launchpad").join("skills"),
            "enabled-project-skill",
            None,
        );
        write_launchpad_skill(
            &project_dir.join(".launchpad").join("skills"),
            "disabled-project-skill",
            None,
        );

        let focus = project_dir.to_string_lossy().into_owned();
        let key = canonical_project_key(&focus);

        let mut agent = make_launchpad_agent("a");
        agent
            .enabled_launchpad_project_skills
            .insert(key, vec!["enabled-project-skill".to_string()]);

        let effective = resolve_effective_launchpad_skills(tmp.path(), &agent, Some(&focus));

        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].name, "enabled-project-skill");
    }
}
