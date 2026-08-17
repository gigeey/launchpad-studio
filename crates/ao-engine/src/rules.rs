use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ao_persistence::paths::DataRoot;
use ao_protocol::agent::AgentProfile;
use ao_protocol::rules::{AddedBy, RuleDto, RuleManifest};
use chrono::{DateTime, Utc};

/// Resolves the per-agent rules directory, honouring `agent.home_dir` when
/// set and otherwise falling back to `<data_root>/agent_homes/<agent_id>/rules`.
pub fn resolve_agent_rules_dir(agent: &AgentProfile, data_root: &DataRoot) -> PathBuf {
    agent
        .home_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| data_root.agent_home_dir(&agent.id))
        .join("rules")
}

/// Creates the agent rules directory if it is missing.
pub fn ensure_agent_rules_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Returns the sidecar manifest path for a rule:
/// - Folder bundle: `<bundle>/.manifest.json`.
/// - Per-file (top-level or nested rule): `<parent>/<filename>.manifest.json`.
fn manifest_path_for_dir(dir: &Path) -> PathBuf {
    dir.join(".manifest.json")
}

fn manifest_path_for_file(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    let name = file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    parent.join(format!("{name}.manifest.json"))
}

fn read_manifest(path: &Path) -> Option<RuleManifest> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn read_bundle_manifest(bundle_dir: &Path) -> Option<RuleManifest> {
    read_manifest(&manifest_path_for_dir(bundle_dir))
}

pub fn read_file_manifest(rule_file: &Path) -> Option<RuleManifest> {
    read_manifest(&manifest_path_for_file(rule_file))
}

pub fn write_bundle_manifest(bundle_dir: &Path, manifest: &RuleManifest) -> io::Result<()> {
    let path = manifest_path_for_dir(bundle_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)
}

pub fn write_file_manifest(rule_file: &Path, manifest: &RuleManifest) -> io::Result<()> {
    let path = manifest_path_for_file(rule_file);
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
fn parse_rule_frontmatter(content: &str) -> (Option<String>, Option<String>) {
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

fn default_manifest_for_existing(imported_at: DateTime<Utc>) -> RuleManifest {
    RuleManifest {
        added_by: AddedBy::Agent,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at,
    }
}

/// Converts a path relative to the rules root into a forward-slash rule id.
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
/// folders and common dev artifacts that never contain user-facing rules.
fn should_descend_into(name: &str) -> bool {
    !(name.starts_with('.') || name == "node_modules" || name == "target")
}

/// Rejects rule ids that are empty or contain unsafe components. Allows
/// forward slashes as nested-rule segment separators but forbids empty
/// segments, `.`, `..`, leading/trailing slashes, and backslashes.
pub fn validate_rule_id(id: &str) -> io::Result<()> {
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rule id must not be empty",
        ));
    }
    if id.contains('\\') || id.starts_with('/') || id.ends_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rule id contains invalid characters",
        ));
    }
    for segment in id.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rule id contains invalid segments",
            ));
        }
    }
    Ok(())
}

/// Reads a rule file fully as UTF-8. Returns `None` if the file cannot be
/// read or contains invalid UTF-8 (a warning is logged in the latter case).
fn read_rule_content(path: &Path) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => Some(s),
            Err(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "skipping rule file: invalid UTF-8",
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "skipping rule file: read failed",
            );
            None
        }
    }
}

/// Builds a DTO for a single rule `.md` file under `rules_dir`. Inherited
/// fields come from `parent_manifest`; the rule's own sibling
/// `<filename>.manifest.json`, if present, contributes the `enabled` override.
fn rule_file_to_dto(
    rules_dir: &Path,
    file: &Path,
    parent_manifest: &RuleManifest,
) -> Option<RuleDto> {
    let rel = file.strip_prefix(rules_dir).ok()?;
    let id = rel_path_to_id(rel);
    if id.is_empty() {
        return None;
    }
    let content = read_rule_content(file)?;
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (title, description) = parse_rule_frontmatter(&content);
    let (mtime, _) = file_times(file);

    let own = read_file_manifest(file);
    let enabled = own
        .as_ref()
        .map(|m| m.enabled)
        .unwrap_or(parent_manifest.enabled);

    Some(RuleDto {
        id,
        title: title.unwrap_or(stem),
        description: description.unwrap_or_default(),
        added_by: parent_manifest.added_by,
        source_url: parent_manifest.source_url.clone(),
        auto_sync: parent_manifest.auto_sync,
        enabled,
        updated_on: mtime,
        added_on: parent_manifest.imported_at,
        content,
    })
}

/// Declared scan target for [`walk_bundle_for_rules`]. Callers must name what
/// they want scanned — a specific directory or a specific list of files —
/// instead of handing over a bundle root and relying on recursion to pick up
/// whatever is there (which used to turn root-level READMEs into rules).
pub enum RulePathSpec<'a> {
    /// Walk `.md` files under this directory. Recurses into subdirectories so
    /// nested rules inside the declared path are still discovered.
    Dir(&'a Path),
    /// Treat each listed path as a rule file. Non-existent or non-`.md` paths
    /// are skipped silently so a partial manifest never poisons the import.
    Files(&'a [PathBuf]),
}

/// Emits a `RuleDto` for every rule file named by `spec`. For `Dir`, recurses
/// into subdirectories but skips hidden folders, `node_modules`, and `target`.
/// A missing directory is not an error — callers get an empty result.
fn walk_bundle_for_rules(
    rules_dir: &Path,
    spec: RulePathSpec<'_>,
    parent_manifest: &RuleManifest,
    out: &mut Vec<RuleDto>,
) {
    match spec {
        RulePathSpec::Dir(dir) => walk_rules_dir_recursive(rules_dir, dir, parent_manifest, out),
        RulePathSpec::Files(files) => {
            for file in files {
                let is_md = file
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("md"))
                    .unwrap_or(false);
                if !is_md {
                    continue;
                }
                if !file.is_file() {
                    continue;
                }
                if let Some(dto) = rule_file_to_dto(rules_dir, file, parent_manifest) {
                    out.push(dto);
                }
            }
        }
    }
}

fn walk_rules_dir_recursive(
    rules_dir: &Path,
    folder: &Path,
    parent_manifest: &RuleManifest,
    out: &mut Vec<RuleDto>,
) {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if ft.is_dir() {
            if !should_descend_into(name) {
                continue;
            }
            walk_rules_dir_recursive(rules_dir, &path, parent_manifest, out);
        } else if ft.is_file() {
            let is_md = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if !is_md {
                continue;
            }
            if let Some(dto) = rule_file_to_dto(rules_dir, &path, parent_manifest) {
                out.push(dto);
            }
        }
    }
}

/// Deletes a top-level rule bundle or flat top-level rule file.
///
/// `rule_id` must be a top-level id (no `/`). Nested ids are rejected with
/// `io::ErrorKind::InvalidInput`. A missing top-level entry returns
/// `io::ErrorKind::NotFound`.
///
/// For folder bundles the whole directory is removed recursively (cascade
/// delete of every nested rule). For flat top-level `.md` rules the file
/// and its sibling `<filename>.manifest.json` (if any) are removed.
pub fn delete_rule(rules_dir: &Path, rule_id: &str) -> io::Result<()> {
    validate_rule_id(rule_id)?;

    if rule_id.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "nested rules cannot be deleted directly — disable it or delete the top-level bundle",
        ));
    }

    let target = rules_dir.join(rule_id);
    let meta = match std::fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("rule '{rule_id}' not found"),
            ));
        }
        Err(e) => return Err(e),
    };

    if meta.file_type().is_dir() {
        std::fs::remove_dir_all(&target)?;
        return Ok(());
    }

    let is_md = target
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md"))
        .unwrap_or(false);
    if !is_md {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("rule '{rule_id}' not found"),
        ));
    }
    std::fs::remove_file(&target)?;
    let sidecar = manifest_path_for_file(&target);
    if sidecar.exists() {
        std::fs::remove_file(&sidecar)?;
    }
    Ok(())
}

/// Fields that may be updated on an existing rule's manifest.
/// Unset fields are preserved as-is on write.
#[derive(Debug, Clone, Copy, Default)]
pub struct RulePatch {
    pub enabled: Option<bool>,
    pub auto_sync: Option<bool>,
}

/// Applies `patch` to the rule identified by `rule_id`. Only the provided
/// fields are updated; untouched fields are preserved. Returns the fresh DTO.
///
/// Routing of the manifest write depends on the id shape:
/// - **Top-level folder bundle** (no `/`, points to a directory): updates
///   `<bundle>/.manifest.json`. Returns the first rule (sorted by id) found in
///   the bundle.
/// - **Top-level flat `.md`** (no `/`, points to an `.md` file): updates the
///   sibling `<filename>.manifest.json`.
/// - **Nested rule** (id contains `/`): writes a sibling
///   `<parent>/<filename>.manifest.json` carrying the per-rule `enabled`
///   override. The parent bundle's manifest is untouched.
///
/// Errors:
/// - `io::ErrorKind::NotFound` — rule does not exist.
/// - `io::ErrorKind::InvalidInput` — `auto_sync=true` on a nested rule
///   (toggle it on the bundle instead), `auto_sync=true` on a non-github
///   top-level rule, or an unsafe `rule_id`.
pub fn patch_rule(
    rules_dir: &Path,
    rule_id: &str,
    patch: RulePatch,
) -> io::Result<RuleDto> {
    validate_rule_id(rule_id)?;

    if rule_id.contains('/') {
        if let Some(true) = patch.auto_sync {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "auto_sync can only be toggled on the top-level bundle",
            ));
        }

        let nested_path = rules_dir.join(rule_id);
        let meta = match std::fs::symlink_metadata(&nested_path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("rule '{rule_id}' not found"),
                ));
            }
            Err(e) => return Err(e),
        };
        if !meta.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("rule '{rule_id}' not found"),
            ));
        }

        let top_segment = rule_id.split('/').next().unwrap_or("");
        let top_bundle = rules_dir.join(top_segment);
        let (_, parent_ctime) = file_times(&top_bundle);
        let parent_manifest = read_bundle_manifest(&top_bundle)
            .unwrap_or_else(|| default_manifest_for_existing(parent_ctime));

        // Per-rule override stores enabled; other fields are mirrored from the
        // parent so a stand-alone read of the sidecar stays internally
        // consistent. Only `enabled` is read back by `rule_file_to_dto`.
        let mut own_manifest = read_file_manifest(&nested_path).unwrap_or(RuleManifest {
            added_by: parent_manifest.added_by,
            enabled: parent_manifest.enabled,
            auto_sync: false,
            source_url: parent_manifest.source_url.clone(),
            imported_at: parent_manifest.imported_at,
        });

        if let Some(enabled) = patch.enabled {
            own_manifest.enabled = enabled;
        }
        write_file_manifest(&nested_path, &own_manifest)?;

        rule_file_to_dto(rules_dir, &nested_path, &parent_manifest).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "failed to build RuleDto after patch")
        })
    } else {
        let target = rules_dir.join(rule_id);
        let meta = match std::fs::symlink_metadata(&target) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("rule '{rule_id}' not found"),
                ));
            }
            Err(e) => return Err(e),
        };

        if meta.file_type().is_dir() {
            let (_, ctime) = file_times(&target);
            let mut manifest = read_bundle_manifest(&target)
                .unwrap_or_else(|| default_manifest_for_existing(ctime));

            if let Some(enabled) = patch.enabled {
                manifest.enabled = enabled;
            }
            if let Some(auto_sync) = patch.auto_sync {
                if auto_sync && manifest.added_by != AddedBy::Github {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "auto_sync can only be enabled for github-imported rules",
                    ));
                }
                manifest.auto_sync = auto_sync;
            }
            write_bundle_manifest(&target, &manifest)?;

            scan_bundle(rules_dir, &target).into_iter().next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("rule '{rule_id}' is empty"),
                )
            })
        } else if meta.file_type().is_file() {
            let is_md = target
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if !is_md {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("rule '{rule_id}' not found"),
                ));
            }
            let (_, ctime) = file_times(&target);
            let mut manifest = read_file_manifest(&target)
                .unwrap_or_else(|| default_manifest_for_existing(ctime));

            if let Some(enabled) = patch.enabled {
                manifest.enabled = enabled;
            }
            if let Some(auto_sync) = patch.auto_sync {
                if auto_sync && manifest.added_by != AddedBy::Github {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "auto_sync can only be enabled for github-imported rules",
                    ));
                }
                manifest.auto_sync = auto_sync;
            }
            write_file_manifest(&target, &manifest)?;

            rule_file_to_dto(rules_dir, &target, &manifest).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "failed to build RuleDto after patch")
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("rule '{rule_id}' not found"),
            ))
        }
    }
}

/// Lists every rule discovered under the agent's `rules/` directory.
///
/// Top-level entries are folder bundles or flat `.md` files. Folder bundles
/// inherit provenance from a `.manifest.json` at their root; missing
/// manifests fall back to `added_by=Agent`. Flat top-level `.md` rules use
/// their own sibling `<filename>.manifest.json` for everything (no parent to
/// inherit from). Returns `Ok(vec![])` when the directory is missing.
pub fn list_rules(agent: &AgentProfile, data_root: &DataRoot) -> io::Result<Vec<RuleDto>> {
    let rules_dir = resolve_agent_rules_dir(agent, data_root);
    Ok(scan_rules_dir(&rules_dir))
}

/// Scans `rules_dir` directly. Used by both `list_rules` and tests.
pub fn scan_rules_dir(rules_dir: &Path) -> Vec<RuleDto> {
    let entries = match std::fs::read_dir(rules_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut rules = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };

        if ft.is_dir() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !should_descend_into(name) {
                continue;
            }
            let (_, ctime) = file_times(&path);
            let manifest =
                read_bundle_manifest(&path).unwrap_or_else(|| default_manifest_for_existing(ctime));
            walk_bundle_for_rules(rules_dir, RulePathSpec::Dir(&path), &manifest, &mut rules);
        } else if ft.is_file() {
            let is_md = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if !is_md {
                continue;
            }
            let (_, ctime) = file_times(&path);
            // Top-level flat rule: use its own per-file manifest if present;
            // otherwise default. There is no parent bundle to inherit from.
            let parent_manifest = read_file_manifest(&path)
                .unwrap_or_else(|| default_manifest_for_existing(ctime));
            if let Some(dto) = rule_file_to_dto(rules_dir, &path, &parent_manifest) {
                rules.push(dto);
            }
        }
    }

    rules.sort_by(|a, b| a.id.cmp(&b.id));
    rules
}

// TODO: extract `unique_folder_name` and `copy_dir_recursive` to a shared util
// crate — they are currently duplicated from `ao_engine::skills`.
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

fn ensure_curl_available() -> io::Result<()> {
    std::process::Command::new("curl")
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "curl not found on PATH"))
}

/// Derives `(filename, safe_stem)` from a URL whose path ends in a `.md`
/// file. `filename` preserves the URL's original casing; `safe_stem` is
/// slugified for use as the bundle folder name.
fn derive_link_filename(url: &str) -> io::Result<(String, String)> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let last_segment = without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    if last_segment.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "url has no filename component",
        ));
    }
    let ext = Path::new(last_segment)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    if !ext.eq_ignore_ascii_case("md") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "url must end in a .md filename",
        ));
    }
    let stem = Path::new(last_segment)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut safe = ao_protocol::slug::slugify(&stem);
    if safe.is_empty() {
        safe = "rule".to_string();
    }
    Ok((last_segment.to_string(), safe))
}

/// Scans a single bundle folder and returns every rule discovered inside it.
/// Used by the import endpoints to report what the import surfaced.
fn scan_bundle(rules_dir: &Path, bundle: &Path) -> Vec<RuleDto> {
    let (_, ctime) = file_times(bundle);
    let manifest =
        read_bundle_manifest(bundle).unwrap_or_else(|| default_manifest_for_existing(ctime));
    let mut out = Vec::new();
    walk_bundle_for_rules(rules_dir, RulePathSpec::Dir(bundle), &manifest, &mut out);
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Copies `src` (a single `.md` file) into
/// `agent_rules_dir/<unique-stem>/<filename>` and writes a bundle
/// `.manifest.json` with `added_by=user`. Returns the rules discovered in
/// the new bundle (one entry).
///
/// Errors with `io::ErrorKind::InvalidInput` when `src` does not have a `.md`
/// extension or is not a file.
pub fn import_file_as_rule(agent_rules_dir: &Path, src: &Path) -> io::Result<Vec<RuleDto>> {
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
    let file_name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?
        .to_string_lossy()
        .into_owned();

    ensure_agent_rules_dir(agent_rules_dir)?;

    let bundle_name = unique_folder_name(agent_rules_dir, &stem);
    let bundle = agent_rules_dir.join(&bundle_name);
    std::fs::create_dir_all(&bundle)?;
    std::fs::copy(src, bundle.join(&file_name))?;

    let manifest = RuleManifest {
        added_by: AddedBy::User,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at: Utc::now(),
    };
    write_bundle_manifest(&bundle, &manifest)?;

    Ok(scan_bundle(agent_rules_dir, &bundle))
}

/// Copies `src` (a directory) recursively into
/// `agent_rules_dir/<unique-name>/` and writes a bundle `.manifest.json`
/// with `added_by=user`. Returns every rule discovered in the imported
/// bundle.
///
/// Errors with `io::ErrorKind::InvalidInput` when `src` is not a directory.
pub fn import_folder_as_rule(agent_rules_dir: &Path, src: &Path) -> io::Result<Vec<RuleDto>> {
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

    ensure_agent_rules_dir(agent_rules_dir)?;

    let target_name = unique_folder_name(agent_rules_dir, &src_name);
    let target = agent_rules_dir.join(&target_name);
    copy_dir_recursive(src, &target)?;

    let manifest = RuleManifest {
        added_by: AddedBy::User,
        enabled: true,
        auto_sync: false,
        source_url: None,
        imported_at: Utc::now(),
    };
    write_bundle_manifest(&target, &manifest)?;

    Ok(scan_bundle(agent_rules_dir, &target))
}

/// Downloads a single `.md` file via HTTP GET (using the system `curl`)
/// and places it at `agent_rules_dir/<unique-stem>/<filename>.md`. Writes a
/// bundle `.manifest.json` with `added_by=link` and `source_url=<url>`.
///
/// Rejects URLs whose path does not end in a `.md` filename.
pub fn import_link_as_rule(agent_rules_dir: &Path, url: &str) -> io::Result<Vec<RuleDto>> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "url must not be empty",
        ));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "url must start with http:// or https://",
        ));
    }
    let (filename, safe_stem) = derive_link_filename(trimmed)?;
    ensure_curl_available()?;
    ensure_agent_rules_dir(agent_rules_dir)?;

    let bundle_name = unique_folder_name(agent_rules_dir, &safe_stem);
    let bundle = agent_rules_dir.join(&bundle_name);
    std::fs::create_dir_all(&bundle)?;
    let target_file = bundle.join(&filename);

    let output = match std::process::Command::new("curl")
        .arg("-fsSL")
        .arg("-o")
        .arg(&target_file)
        .arg(trimmed)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&bundle);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("failed to run curl: {e}"),
            ));
        }
    };

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&bundle);
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
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("download failed: {}", tail.trim()),
        ));
    }

    let manifest = RuleManifest {
        added_by: AddedBy::Link,
        enabled: true,
        auto_sync: false,
        source_url: Some(trimmed.to_string()),
        imported_at: Utc::now(),
    };
    write_bundle_manifest(&bundle, &manifest)?;

    Ok(scan_bundle(agent_rules_dir, &bundle))
}

/// Runs `git pull` against every top-level bundle whose manifest has
/// `added_by=github` and `auto_sync=true`. Bundles without a `.git` directory
/// (e.g. imported with a subpath) are skipped. Per-bundle pull failures are
/// logged without aborting. Returns the freshly scanned list after all pull
/// attempts. Safe to call when the directory is missing (returns an empty vec).
pub fn refresh_agent_rules(rules_dir: &Path, agent_id: &str) -> Vec<RuleDto> {
    let entries = match std::fs::read_dir(rules_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let bundle = entry.path();
        let manifest = match read_bundle_manifest(&bundle) {
            Some(m) => m,
            None => continue,
        };
        if manifest.added_by != AddedBy::Github || !manifest.auto_sync {
            continue;
        }
        if !bundle.join(".git").exists() {
            tracing::debug!(
                agent_id = %agent_id,
                bundle = %bundle.display(),
                "skipping refresh: no .git in bundle (subpath-imported)",
            );
            continue;
        }
        match std::process::Command::new("git")
            .arg("pull")
            .current_dir(&bundle)
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!(
                    agent_id = %agent_id,
                    bundle = %bundle.display(),
                    "git pull succeeded for auto-sync rule bundle",
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
                    bundle = %bundle.display(),
                    stderr = %tail.trim(),
                    "git pull failed for auto-sync rule bundle",
                );
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    bundle = %bundle.display(),
                    error = %e,
                    "git pull could not be invoked for auto-sync rule bundle",
                );
            }
        }
    }
    scan_rules_dir(rules_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use std::collections::HashMap;

    fn sample_agent(id: &str, home_dir: Option<String>) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: "Test".to_string(),
            description: "Test agent".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "claude".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Json,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
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
            home_dir,
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

    fn sample_manifest(added_by: AddedBy) -> RuleManifest {
        RuleManifest {
            added_by,
            enabled: true,
            auto_sync: false,
            source_url: None,
            imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn resolves_default_rules_dir_from_data_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let agent = sample_agent("agent-a", None);

        let rules_dir = resolve_agent_rules_dir(&agent, &data_root);

        assert_eq!(
            rules_dir,
            tmp.path().join("agent_homes").join("agent-a").join("rules"),
        );
    }

    #[test]
    fn resolves_override_rules_dir_when_home_dir_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path().join("unused"));
        let custom_home = tmp.path().join("custom-home");
        let agent = sample_agent("agent-b", Some(custom_home.to_string_lossy().into_owned()));

        let rules_dir = resolve_agent_rules_dir(&agent, &data_root);

        assert_eq!(rules_dir, custom_home.join("rules"));
    }

    #[test]
    fn ensure_agent_rules_dir_creates_missing_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("agent_homes").join("a").join("rules");
        assert!(!target.exists());

        ensure_agent_rules_dir(&target).unwrap();

        assert!(target.is_dir());
    }

    #[test]
    fn list_rules_missing_dir_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let agent = sample_agent("agent-empty", None);

        let result = list_rules(&agent, &data_root).unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn scan_empty_dir_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(scan_rules_dir(tmp.path()).is_empty());
    }

    #[test]
    fn walk_bundle_for_rules_dir_scans_only_declared_path_ignoring_root_readme() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path();
        // Declared rules path:
        std::fs::create_dir_all(repo.join("rules")).unwrap();
        std::fs::write(repo.join("rules").join("a.md"), "alpha").unwrap();
        std::fs::write(repo.join("rules").join("b.md"), "beta").unwrap();
        // Root-level README (must be ignored — this is the regression we're fixing):
        std::fs::write(repo.join("README.md"), "# readme").unwrap();
        std::fs::write(repo.join("CHANGELOG.md"), "changelog").unwrap();

        let manifest = sample_manifest(AddedBy::User);
        let mut out = Vec::new();
        walk_bundle_for_rules(
            &repo.join("rules"),
            RulePathSpec::Dir(&repo.join("rules")),
            &manifest,
            &mut out,
        );
        out.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "a.md");
        assert_eq!(out[1].id, "b.md");
    }

    #[test]
    fn walk_bundle_for_rules_dir_missing_returns_empty_not_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");

        let manifest = sample_manifest(AddedBy::User);
        let mut out = Vec::new();
        walk_bundle_for_rules(
            tmp.path(),
            RulePathSpec::Dir(&missing),
            &manifest,
            &mut out,
        );

        assert!(out.is_empty());
    }

    #[test]
    fn walk_bundle_for_rules_files_returns_each_declared_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        std::fs::write(rules_dir.join("one.md"), "one body").unwrap();
        std::fs::write(rules_dir.join("two.md"), "two body").unwrap();
        // A README sitting alongside should NOT be picked up — Files mode
        // never scans, it only loads what's in the list.
        std::fs::write(rules_dir.join("README.md"), "readme").unwrap();

        let files = vec![rules_dir.join("one.md"), rules_dir.join("two.md")];
        let manifest = sample_manifest(AddedBy::User);
        let mut out = Vec::new();
        walk_bundle_for_rules(rules_dir, RulePathSpec::Files(&files), &manifest, &mut out);
        out.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "one.md");
        assert_eq!(out[1].id, "two.md");
    }

    #[test]
    fn bundle_with_root_and_nested_md_yields_n_entries_with_correct_ids() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();

        // Top-level bundle with a root .md and nested .md files.
        let bundle = rules_dir.join("my-bundle");
        std::fs::create_dir_all(bundle.join("inner")).unwrap();
        std::fs::write(bundle.join("intro.md"), "# Intro\n\nbody").unwrap();
        std::fs::write(
            bundle.join("inner").join("strict.md"),
            "---\ntitle: \"Strict Mode\"\ndescription: \"be strict\"\n---\nbody",
        )
        .unwrap();
        std::fs::write(bundle.join("inner").join("loose.md"), "loose body").unwrap();
        write_bundle_manifest(
            &bundle,
            &RuleManifest {
                added_by: AddedBy::Github,
                source_url: Some("https://github.com/owner/rules".to_string()),
                auto_sync: true,
                ..sample_manifest(AddedBy::Github)
            },
        )
        .unwrap();

        let mut dtos = scan_rules_dir(rules_dir);
        dtos.sort_by(|a, b| a.id.cmp(&b.id));

        assert_eq!(dtos.len(), 3, "expected 3 rules, got: {:?}", dtos);
        assert_eq!(dtos[0].id, "my-bundle/inner/loose.md");
        assert_eq!(dtos[0].title, "loose"); // falls back to filename stem
        assert_eq!(dtos[0].content, "loose body");
        assert_eq!(dtos[1].id, "my-bundle/inner/strict.md");
        assert_eq!(dtos[1].title, "Strict Mode");
        assert_eq!(dtos[1].description, "be strict");
        assert_eq!(dtos[2].id, "my-bundle/intro.md");
        // All three inherit from the bundle manifest.
        for dto in &dtos {
            assert_eq!(dto.added_by, AddedBy::Github);
            assert_eq!(
                dto.source_url.as_deref(),
                Some("https://github.com/owner/rules")
            );
            assert!(dto.auto_sync);
            assert!(dto.enabled);
        }
    }

    #[test]
    fn manifest_inheritance_on_nested_files_is_overridden_by_per_file_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        let nested_dir = bundle.join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested_file = nested_dir.join("rule.md");
        std::fs::write(&nested_file, "body").unwrap();

        write_bundle_manifest(
            &bundle,
            &RuleManifest {
                enabled: true,
                ..sample_manifest(AddedBy::User)
            },
        )
        .unwrap();
        // Per-file override: disable just this nested rule.
        write_file_manifest(
            &nested_file,
            &RuleManifest {
                enabled: false,
                ..sample_manifest(AddedBy::User)
            },
        )
        .unwrap();

        let dtos = scan_rules_dir(rules_dir);
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "bundle/nested/rule.md");
        assert!(!dtos[0].enabled, "per-file enabled override should win");
        // Inherited fields still come from the bundle.
        assert_eq!(dtos[0].added_by, AddedBy::User);
    }

    #[test]
    fn invalid_utf8_file_is_skipped_with_warning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        // Valid + invalid UTF-8 file.
        std::fs::write(bundle.join("good.md"), "good body").unwrap();
        std::fs::write(bundle.join("bad.md"), &[0xFF, 0xFE, 0x00, 0x6E][..]).unwrap();

        let dtos = scan_rules_dir(rules_dir);
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "bundle/good.md");
    }

    #[test]
    fn validate_rule_id_accepts_nested_ids() {
        validate_rule_id("simple.md").unwrap();
        validate_rule_id("bundle/inner/strict.md").unwrap();
        validate_rule_id("bundle").unwrap();
    }

    #[test]
    fn validate_rule_id_rejects_traversal_and_unsafe_chars() {
        assert!(validate_rule_id("").is_err());
        assert!(validate_rule_id("/leading").is_err());
        assert!(validate_rule_id("trailing/").is_err());
        assert!(validate_rule_id("a//b").is_err());
        assert!(validate_rule_id("a/./b").is_err());
        assert!(validate_rule_id("a/../b").is_err());
        assert!(validate_rule_id("a\\b").is_err());
    }

    #[test]
    fn walker_skips_hidden_node_modules_and_target_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle");
        std::fs::create_dir_all(bundle.join(".git")).unwrap();
        std::fs::create_dir_all(bundle.join("node_modules").join("pkg")).unwrap();
        std::fs::create_dir_all(bundle.join("target")).unwrap();
        std::fs::create_dir_all(bundle.join("ok")).unwrap();
        std::fs::write(bundle.join(".git").join("hidden.md"), "no").unwrap();
        std::fs::write(bundle.join("node_modules").join("pkg").join("nope.md"), "no").unwrap();
        std::fs::write(bundle.join("target").join("nope.md"), "no").unwrap();
        std::fs::write(bundle.join("ok").join("yes.md"), "yes body").unwrap();

        let dtos = scan_rules_dir(rules_dir);
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "bundle/ok/yes.md");
    }

    #[test]
    fn top_level_flat_md_is_a_rule_with_default_provenance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        std::fs::write(rules_dir.join("flat.md"), "flat body").unwrap();

        let dtos = scan_rules_dir(rules_dir);
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "flat.md");
        assert_eq!(dtos[0].title, "flat");
        assert_eq!(dtos[0].added_by, AddedBy::Agent);
        assert!(dtos[0].enabled);
        assert_eq!(dtos[0].content, "flat body");
    }

    #[test]
    fn list_rules_uses_resolved_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let agent = sample_agent("ag", None);

        let rules_dir = resolve_agent_rules_dir(&agent, &data_root);
        ensure_agent_rules_dir(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("alpha.md"), "alpha").unwrap();

        let dtos = list_rules(&agent, &data_root).unwrap();
        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "alpha.md");
    }

    #[test]
    fn manifest_round_trip_for_bundle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bundle = tmp.path().join("b");
        std::fs::create_dir_all(&bundle).unwrap();
        let manifest = sample_manifest(AddedBy::Github);
        write_bundle_manifest(&bundle, &manifest).unwrap();
        let loaded = read_bundle_manifest(&bundle).expect("manifest present");
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn delete_rule_top_level_bundle_cascades() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle-x");
        std::fs::create_dir_all(bundle.join("inner")).unwrap();
        std::fs::write(bundle.join("root.md"), "root").unwrap();
        std::fs::write(bundle.join("inner").join("nested.md"), "nested").unwrap();
        write_bundle_manifest(&bundle, &sample_manifest(AddedBy::User)).unwrap();

        delete_rule(rules_dir, "bundle-x").unwrap();

        assert!(!bundle.exists(), "bundle dir should be removed");
    }

    #[test]
    fn delete_rule_top_level_flat_md_removes_file_and_sidecar() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        std::fs::write(rules_dir.join("flat.md"), "flat").unwrap();
        write_file_manifest(
            &rules_dir.join("flat.md"),
            &sample_manifest(AddedBy::User),
        )
        .unwrap();
        let sidecar = rules_dir.join("flat.md.manifest.json");
        assert!(sidecar.exists());

        delete_rule(rules_dir, "flat.md").unwrap();

        assert!(!rules_dir.join("flat.md").exists());
        assert!(!sidecar.exists());
    }

    #[test]
    fn delete_rule_nested_id_returns_invalid_input() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle");
        std::fs::create_dir_all(bundle.join("inner")).unwrap();
        std::fs::write(bundle.join("inner").join("rule.md"), "x").unwrap();

        let err = delete_rule(rules_dir, "bundle/inner/rule.md").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("nested rules cannot be deleted directly"));
        // Nested file must still exist.
        assert!(bundle.join("inner").join("rule.md").exists());
    }

    #[test]
    fn delete_rule_unknown_id_returns_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let err = delete_rule(rules_dir, "missing").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn delete_rule_rejects_unsafe_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(delete_rule(tmp.path(), "").is_err());
        assert!(delete_rule(tmp.path(), "..").is_err());
        assert!(delete_rule(tmp.path(), "a\\b").is_err());
    }

    #[test]
    fn manifest_round_trip_for_per_file_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("rule.md");
        std::fs::write(&file, "body").unwrap();
        let manifest = RuleManifest {
            enabled: false,
            ..sample_manifest(AddedBy::User)
        };
        write_file_manifest(&file, &manifest).unwrap();
        let sidecar = tmp.path().join("rule.md.manifest.json");
        assert!(sidecar.exists());
        let loaded = read_file_manifest(&file).expect("manifest present");
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn import_file_rejects_non_md_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("notes.txt");
        std::fs::write(&src, "hello").unwrap();
        let rules_dir = tmp.path().join("rules");

        let err = import_file_as_rule(&rules_dir, &src).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn import_file_happy_path_wraps_in_bundle_with_user_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("strict.md");
        std::fs::write(
            &src,
            "---\ntitle: \"Strict\"\ndescription: \"be strict\"\n---\nbody",
        )
        .unwrap();
        let rules_dir = tmp.path().join("rules");

        let dtos = import_file_as_rule(&rules_dir, &src).unwrap();

        assert_eq!(dtos.len(), 1);
        assert_eq!(dtos[0].id, "strict/strict.md");
        assert_eq!(dtos[0].title, "Strict");
        assert_eq!(dtos[0].added_by, AddedBy::User);
        assert!(dtos[0].enabled);
        assert!(!dtos[0].auto_sync);
        assert_eq!(dtos[0].content, "---\ntitle: \"Strict\"\ndescription: \"be strict\"\n---\nbody");
        assert!(rules_dir.join("strict").join("strict.md").is_file());
        let bundle_manifest = read_bundle_manifest(&rules_dir.join("strict")).unwrap();
        assert_eq!(bundle_manifest.added_by, AddedBy::User);
    }

    #[test]
    fn import_file_collision_appends_suffix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("tip.md");
        std::fs::write(&src, "body").unwrap();
        let rules_dir = tmp.path().join("rules");

        let first = import_file_as_rule(&rules_dir, &src).unwrap();
        let second = import_file_as_rule(&rules_dir, &src).unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "tip/tip.md");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, "tip-2/tip.md");
        assert!(rules_dir.join("tip").join("tip.md").is_file());
        assert!(rules_dir.join("tip-2").join("tip.md").is_file());
    }

    #[test]
    fn import_folder_recursive_copy_and_scan() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("bundle");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("root.md"), "root body").unwrap();
        std::fs::write(
            src.join("inner").join("strict.md"),
            "---\ntitle: \"Strict\"\n---\nbody",
        )
        .unwrap();
        std::fs::write(src.join("inner").join("loose.md"), "loose body").unwrap();
        let rules_dir = tmp.path().join("rules");

        let dtos = import_folder_as_rule(&rules_dir, &src).unwrap();

        assert_eq!(dtos.len(), 3, "expected 3 rules, got: {:?}", dtos);
        let ids: Vec<_> = dtos.iter().map(|d| d.id.clone()).collect();
        assert_eq!(
            ids,
            vec![
                "bundle/inner/loose.md".to_string(),
                "bundle/inner/strict.md".to_string(),
                "bundle/root.md".to_string(),
            ]
        );
        for dto in &dtos {
            assert_eq!(dto.added_by, AddedBy::User);
            assert!(dto.enabled);
            assert!(!dto.auto_sync);
        }
        assert!(rules_dir.join("bundle").join(".manifest.json").is_file());
    }

    #[test]
    fn import_folder_rejects_missing_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("does-not-exist");
        let rules_dir = tmp.path().join("rules");

        let err = import_folder_as_rule(&rules_dir, &src).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn import_folder_collision_appends_suffix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("pack");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.md"), "a").unwrap();
        let rules_dir = tmp.path().join("rules");

        let first = import_folder_as_rule(&rules_dir, &src).unwrap();
        let second = import_folder_as_rule(&rules_dir, &src).unwrap();

        assert_eq!(first[0].id, "pack/a.md");
        assert_eq!(second[0].id, "pack-2/a.md");
    }

    #[test]
    fn import_link_rejects_non_md_url() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path().join("rules");
        let err = import_link_as_rule(&rules_dir, "https://example.com/notes.txt").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn import_link_rejects_non_http_scheme() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path().join("rules");
        let err = import_link_as_rule(&rules_dir, "ftp://example.com/rules.md").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn derive_link_filename_happy_path() {
        let (file, stem) =
            derive_link_filename("https://example.com/raw/My_Rules.md").unwrap();
        assert_eq!(file, "My_Rules.md");
        assert_eq!(stem, "my-rules");
    }

    #[test]
    fn derive_link_filename_strips_query_and_fragment() {
        let (file, stem) =
            derive_link_filename("https://example.com/my.md?a=1#frag").unwrap();
        assert_eq!(file, "my.md");
        assert_eq!(stem, "my");
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
    fn refresh_returns_empty_for_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let out = refresh_agent_rules(&missing, "agent-a");
        assert!(out.is_empty());
    }

    #[test]
    fn refresh_skips_non_github_bundles() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("user-pack");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("a.md"), "a").unwrap();
        write_bundle_manifest(&bundle, &sample_manifest(AddedBy::User)).unwrap();

        let out = refresh_agent_rules(rules_dir, "agent-a");

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "user-pack/a.md");
        assert_eq!(out[0].added_by, AddedBy::User);
    }

    #[test]
    fn refresh_skips_github_bundle_without_auto_sync() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("gh-off");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("a.md"), "a").unwrap();
        write_bundle_manifest(
            &bundle,
            &RuleManifest {
                added_by: AddedBy::Github,
                enabled: true,
                auto_sync: false,
                source_url: Some("https://github.com/owner/repo".to_string()),
                imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            },
        )
        .unwrap();

        let out = refresh_agent_rules(rules_dir, "agent-a");

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "gh-off/a.md");
    }

    #[test]
    fn patch_rule_top_level_bundle_toggles_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle");
        std::fs::create_dir_all(bundle.join("inner")).unwrap();
        std::fs::write(bundle.join("inner").join("a.md"), "a body").unwrap();
        write_bundle_manifest(&bundle, &sample_manifest(AddedBy::User)).unwrap();

        let dto = patch_rule(
            rules_dir,
            "bundle",
            RulePatch {
                enabled: Some(false),
                auto_sync: None,
            },
        )
        .unwrap();

        // The first (and only) rule under the bundle now reflects the new
        // enabled state via parent-manifest inheritance.
        assert_eq!(dto.id, "bundle/inner/a.md");
        assert!(!dto.enabled);
        let manifest = read_bundle_manifest(&bundle).unwrap();
        assert!(!manifest.enabled);
    }

    #[test]
    fn patch_rule_top_level_flat_md_writes_sidecar() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        std::fs::write(rules_dir.join("flat.md"), "flat body").unwrap();

        let dto = patch_rule(
            rules_dir,
            "flat.md",
            RulePatch {
                enabled: Some(false),
                auto_sync: None,
            },
        )
        .unwrap();

        assert_eq!(dto.id, "flat.md");
        assert!(!dto.enabled);
        let sidecar = rules_dir.join("flat.md.manifest.json");
        assert!(sidecar.exists(), "per-file manifest should be written");
        let manifest = read_file_manifest(&rules_dir.join("flat.md")).unwrap();
        assert!(!manifest.enabled);
    }

    #[test]
    fn patch_rule_nested_writes_per_file_sidecar_and_leaves_parent_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle");
        std::fs::create_dir_all(bundle.join("inner")).unwrap();
        let nested = bundle.join("inner").join("rule.md");
        std::fs::write(&nested, "body").unwrap();
        write_bundle_manifest(
            &bundle,
            &RuleManifest {
                added_by: AddedBy::Github,
                enabled: true,
                auto_sync: true,
                source_url: Some("https://github.com/owner/repo".to_string()),
                imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            },
        )
        .unwrap();

        let dto = patch_rule(
            rules_dir,
            "bundle/inner/rule.md",
            RulePatch {
                enabled: Some(false),
                auto_sync: None,
            },
        )
        .unwrap();

        assert_eq!(dto.id, "bundle/inner/rule.md");
        assert!(!dto.enabled);
        // Nested fields still inherited from the parent.
        assert_eq!(dto.added_by, AddedBy::Github);
        assert!(dto.auto_sync);
        assert_eq!(
            dto.source_url.as_deref(),
            Some("https://github.com/owner/repo")
        );

        // Per-file sidecar exists and disables the rule.
        let sidecar = bundle.join("inner").join("rule.md.manifest.json");
        assert!(sidecar.exists());
        let own = read_file_manifest(&nested).unwrap();
        assert!(!own.enabled);

        // Parent bundle manifest is untouched.
        let parent = read_bundle_manifest(&bundle).unwrap();
        assert!(parent.enabled);
        assert!(parent.auto_sync);
    }

    #[test]
    fn patch_rule_nested_auto_sync_true_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("bundle");
        std::fs::create_dir_all(bundle.join("inner")).unwrap();
        std::fs::write(bundle.join("inner").join("rule.md"), "body").unwrap();

        let err = patch_rule(
            rules_dir,
            "bundle/inner/rule.md",
            RulePatch {
                enabled: None,
                auto_sync: Some(true),
            },
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err
            .to_string()
            .contains("auto_sync can only be toggled on the top-level bundle"));
        // No sidecar should be written.
        assert!(!bundle.join("inner").join("rule.md.manifest.json").exists());
    }

    #[test]
    fn patch_rule_top_level_auto_sync_on_github_bundle_is_allowed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("gh");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("a.md"), "a").unwrap();
        write_bundle_manifest(
            &bundle,
            &RuleManifest {
                added_by: AddedBy::Github,
                enabled: true,
                auto_sync: false,
                source_url: Some("https://github.com/owner/repo".to_string()),
                imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
            },
        )
        .unwrap();

        let dto = patch_rule(
            rules_dir,
            "gh",
            RulePatch {
                enabled: None,
                auto_sync: Some(true),
            },
        )
        .unwrap();

        assert!(dto.auto_sync);
        let manifest = read_bundle_manifest(&bundle).unwrap();
        assert!(manifest.auto_sync);
    }

    #[test]
    fn patch_rule_top_level_auto_sync_on_user_bundle_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        let bundle = rules_dir.join("user-pack");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("a.md"), "a").unwrap();
        write_bundle_manifest(&bundle, &sample_manifest(AddedBy::User)).unwrap();

        let err = patch_rule(
            rules_dir,
            "user-pack",
            RulePatch {
                enabled: None,
                auto_sync: Some(true),
            },
        )
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err
            .to_string()
            .contains("auto_sync can only be enabled for github-imported rules"));
    }

    #[test]
    fn patch_rule_unknown_id_returns_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        std::fs::create_dir_all(rules_dir).unwrap();

        let err = patch_rule(
            rules_dir,
            "missing",
            RulePatch {
                enabled: Some(true),
                auto_sync: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let err_nested = patch_rule(
            rules_dir,
            "bundle/inner/nope.md",
            RulePatch {
                enabled: Some(true),
                auto_sync: None,
            },
        )
        .unwrap_err();
        assert_eq!(err_nested.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn patch_rule_rejects_unsafe_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = patch_rule(
            tmp.path(),
            "..",
            RulePatch {
                enabled: Some(true),
                auto_sync: None,
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn refresh_aggregates_across_multiple_bundles_without_panicking() {
        // Create two github auto_sync bundles that have no .git — refresh
        // must skip the git pull for both (since .git is missing) and still
        // return the full re-scan.
        let tmp = tempfile::TempDir::new().unwrap();
        let rules_dir = tmp.path();
        for name in ["repo-a", "repo-b"] {
            let bundle = rules_dir.join(name);
            std::fs::create_dir_all(&bundle).unwrap();
            std::fs::write(bundle.join("r.md"), "r").unwrap();
            write_bundle_manifest(
                &bundle,
                &RuleManifest {
                    added_by: AddedBy::Github,
                    enabled: true,
                    auto_sync: true,
                    source_url: Some(format!("https://github.com/owner/{name}")),
                    imported_at: DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
                },
            )
            .unwrap();
        }

        let out = refresh_agent_rules(rules_dir, "agent-a");

        let ids: Vec<_> = out.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec!["repo-a/r.md".to_string(), "repo-b/r.md".to_string()]);
    }
}
