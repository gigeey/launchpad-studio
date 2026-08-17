use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::contract_primitives::sha256_hex;
use crate::data_root::DEFAULT_DATA_DIR_NAME;
use crate::error::AoError;

/// Id of the single entry seeded into a freshly-bootstrapped registry, and
/// the `active` value that entry is pointed at by default.
const DEFAULT_WORKSPACE_ID: &str = "default";

/// Fixed set of background colors a workspace's `color` can be assigned
/// from, in the workspace switcher. [`derive_default_color`] indexes into
/// this deterministically by `id`, and the create/rename routes validate
/// (client-side, via the mirrored copy) that a user-picked color comes from
/// the same set. Mirrored verbatim as `WORKSPACE_COLOR_PALETTE` in
/// `frontend/src/lib/api.ts` — the picker must offer exactly these values,
/// so keep both lists in sync by hand if this ever changes.
pub const WORKSPACE_COLOR_PALETTE: [&str; 8] = [
    "#EF4444", "#F97316", "#EAB308", "#22C55E", "#14B8A6", "#3B82F6", "#8B5CF6", "#EC4899",
];

/// Deterministically pick a [`WORKSPACE_COLOR_PALETTE`] entry for `id`, via a
/// sha256 digest of the id string indexed into the palette. Deterministic
/// (same `id` always yields the same color, across processes and restarts)
/// and stable (never recomputed once persisted), so workspaces that predate
/// the `color` field — or that were created without one specified — get
/// distinct, visually-stable colors instead of every entry rendering
/// identically.
pub fn derive_default_color(id: &str) -> &'static str {
    let digest = sha256_hex(id);
    // The digest is a lowercase hex string; its first byte (2 hex chars) is
    // plenty of entropy to index an 8-element palette evenly.
    let first_byte = u8::from_str_radix(&digest[0..2], 16).unwrap_or(0);
    WORKSPACE_COLOR_PALETTE[(first_byte as usize) % WORKSPACE_COLOR_PALETTE.len()]
}

/// Serde default for [`WorkspaceEntry::color`], used only so a registry
/// entry written before this field existed still deserializes instead of
/// failing the whole registry load (see [`AoError::WorkspaceRegistryCorrupt`]
/// and why that's fatal for every mutation route). Returns an empty string —
/// never a real palette color — as a sentinel that lets
/// [`WorkspaceEntry::fill_missing_visual_defaults`] tell "field absent from
/// the JSON" apart from "field present with a real value" after parsing.
/// [`load_registry_from_path_classified`] calls that method on every entry
/// immediately after every parse, so no caller outside this module ever
/// observes an empty `color`.
fn empty_color_default() -> String {
    String::new()
}

/// One data root the user can switch to. `path` is where all per-user state
/// (agents, transcripts, memories, preferences, etc.) lives when this
/// workspace is active — the same layout `resolve_data_root` has always
/// pointed callers at.
///
/// `id` is a stable identifier independent of `path`, so renaming or moving
/// a workspace's directory doesn't change its identity in the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    /// Background color for this workspace in the switcher UI — a hex
    /// string from [`WORKSPACE_COLOR_PALETTE`]. `#[serde(default)]`'d to an
    /// empty-string sentinel (see [`empty_color_default`]) so a
    /// pre-color-field registry entry still deserializes; the sentinel is
    /// replaced with a real, `id`-derived value on load — see
    /// [`WorkspaceEntry::fill_missing_visual_defaults`].
    #[serde(default = "empty_color_default")]
    pub color: String,
    /// Emoji for this workspace in the switcher UI. Genuinely optional:
    /// `None` means the user hasn't chosen one, and the UI is responsible
    /// for rendering a letter avatar in that case rather than this module
    /// manufacturing a placeholder. `#[serde(default)]` (the plain,
    /// no-argument form — `Option<T>`'s own `Default` is already `None`)
    /// so an entry that predates this field, or one written by a version
    /// that omits it because it's unset, both deserialize the same way:
    /// straight to `None`.
    ///
    /// ⚠️ `skip_serializing_if = "Option::is_none"` is REQUIRED here, not
    /// stylistic — do not remove it. An unset emoji MUST be omitted from
    /// the JSON entirely and must NEVER serialize as `"emoji": null`. The
    /// registry file this round-trips through
    /// (`~/.launchpad_studio/workspaces.json`) is shared with a second
    /// worktree/app whose older binary still declares
    /// `#[serde(default = "empty_emoji_default")] pub emoji: String` — a
    /// plain (non-`Option`) `String` field. Serde's `default` only fires
    /// when a field is ABSENT from the JSON; an explicit `null` written
    /// into that older `String` field is a hard deserialize error, and a
    /// registry parse failure is fatal for every mutation route in this
    /// codebase (see [`AoError::WorkspaceRegistryCorrupt`]). An absent
    /// field is forward-compatible with that older binary; a `null` field
    /// would brick its ability to write to the shared registry at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
}

impl WorkspaceEntry {
    /// Replace an empty (i.e. defaulted-away-from-missing, see
    /// [`empty_color_default`]) `color` with a real value, derived via
    /// [`derive_default_color`] from this entry's `id`. A no-op once an
    /// entry has a real `color` — which every entry does immediately after
    /// its first load post-migration, since
    /// [`load_registry_from_path_classified`] calls this on every parse.
    ///
    /// `emoji` is deliberately NOT touched here. Unlike `color`, an unset
    /// emoji is a meaningful, permanent state ("the user hasn't chosen
    /// one") rather than a pre-field-existing migration gap to paper over
    /// — see [`WorkspaceEntry::emoji`].
    pub fn fill_missing_visual_defaults(&mut self) {
        if self.color.is_empty() {
            self.color = derive_default_color(&self.id).to_string();
        }
    }
}

/// On-disk registry of known workspaces and which one is active.
///
/// Lives at a fixed bootstrap path (see [`registry_path`]) rather than
/// inside any workspace itself, because the registry has to be readable
/// before we know which workspace is active — putting it inside a
/// workspace would make that lookup circular.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRegistry {
    pub version: u32,
    /// `id` of the entry in `workspaces` that is currently active. If this
    /// doesn't match any entry, resolution falls through to the next
    /// precedence source rather than erroring.
    pub active: String,
    pub workspaces: Vec<WorkspaceEntry>,
}

impl WorkspaceRegistry {
    /// The registry a fresh install (or a corrupt/missing registry file)
    /// falls back to: a single workspace pointing at `default_root`,
    /// already marked active.
    pub fn default_registry(default_root: PathBuf) -> Self {
        Self {
            version: 1,
            active: DEFAULT_WORKSPACE_ID.to_string(),
            workspaces: vec![WorkspaceEntry {
                id: DEFAULT_WORKSPACE_ID.to_string(),
                name: "Default".to_string(),
                path: default_root,
                color: derive_default_color(DEFAULT_WORKSPACE_ID).to_string(),
                // No emoji manufactured for a fresh install either — the UI
                // renders a letter avatar until the user picks one.
                emoji: None,
            }],
        }
    }

    /// The path of the entry named by `active`, or `None` if `active`
    /// doesn't match any entry (e.g. it was deleted out from under a stale
    /// registry).
    pub fn active_path(&self) -> Option<PathBuf> {
        self.workspaces
            .iter()
            .find(|entry| entry.id == self.active)
            .map(|entry| entry.path.clone())
    }
}

/// Response body for `POST /workspaces`. Flattens [`WorkspaceEntry`] and adds
/// `adopted`, so a client can tell the two outcomes of a create-or-adopt
/// request apart: `false` when `path` was empty (or missing) and got freshly
/// scaffolded via `DataRoot::ensure_directories`; `true` when `path` already
/// contained a valid Launchpad data root and was registered as-is, with
/// nothing on disk touched.
///
/// Deliberately a separate type rather than a field added to
/// [`WorkspaceEntry`] itself — `WorkspaceEntry` is round-tripped through the
/// registry file on every load/save, and `adopted` is meaningless outside
/// the single create-or-adopt response that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateWorkspaceResponse {
    #[serde(flatten)]
    pub workspace: WorkspaceEntry,
    pub adopted: bool,
}

#[cfg(feature = "test-util")]
thread_local! {
    /// Test-only override for [`registry_path`]'s resolved path, so a test can
    /// drive real registry-reading/-writing code (route handlers, not just the
    /// pure `_from_path` helpers already covered below) against a temp file
    /// instead of the real `~/.launchpad_studio/workspaces.json`. Gated behind
    /// the `test-util` cargo feature rather than bare `#[cfg(test)]` because
    /// `registry_path` is called from `ao-server`'s route handlers in a
    /// different crate, and `#[cfg(test)]` items don't survive across a crate
    /// boundary — only this crate's own `cargo test` would see them. `test-util`
    /// is enabled only via `[dev-dependencies]` (see `ao-server/Cargo.toml`),
    /// so it never reaches a release binary and adds no new *runtime*
    /// configuration surface (no env var, no config file field).
    ///
    /// Thread-local rather than a `static`/`OnceLock`: unlike
    /// `LAUNCHPAD_STUDIO_DATA_DIR` (a real env var, genuinely process-wide, see
    /// `data_root::ENV_MUTEX` in that module's tests), this override only ever
    /// needs to be visible to the single thread that set it. Rust's default
    /// test harness runs each `#[test]`/`#[tokio::test]` function on its own
    /// thread — and pins it there for the duration of a synchronous call chain
    /// with no `.await` in between, which is exactly the shape of
    /// `registry_path`'s callers — so this gives perfect test-to-test isolation
    /// with no cross-test lock required.
    static REGISTRY_PATH_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Clears the current thread's [`REGISTRY_PATH_OVERRIDE`] when dropped, so a
/// test can't leak its override into whatever test happens to reuse this
/// thread next. Returned by [`override_registry_path_for_test`]; hold it for
/// as long as the override should apply.
#[cfg(feature = "test-util")]
pub struct RegistryPathOverrideGuard(());

#[cfg(feature = "test-util")]
impl Drop for RegistryPathOverrideGuard {
    fn drop(&mut self) {
        REGISTRY_PATH_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Point [`registry_path`] at `path` for the current thread until the
/// returned guard drops. Test-only — see [`REGISTRY_PATH_OVERRIDE`].
#[cfg(feature = "test-util")]
pub fn override_registry_path_for_test(path: PathBuf) -> RegistryPathOverrideGuard {
    REGISTRY_PATH_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(path));
    RegistryPathOverrideGuard(())
}

/// Fixed bootstrap path for the workspace registry: `~/.launchpad_studio/workspaces.json`.
///
/// This path is intentionally NOT derived from `resolve_data_root` — the
/// registry has to live somewhere known in advance, since it's what tells
/// us which data root is active in the first place.
///
/// Default behavior (no `test-util` override active) is unchanged from
/// before this seam existed — see [`REGISTRY_PATH_OVERRIDE`] for why the
/// override itself can never affect a non-test build.
pub fn registry_path() -> Result<PathBuf, AoError> {
    #[cfg(feature = "test-util")]
    {
        if let Some(path) = REGISTRY_PATH_OVERRIDE.with(|cell| cell.borrow().clone()) {
            return Ok(path);
        }
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| AoError::Internal("Could not determine home directory".into()))?
        .join(DEFAULT_DATA_DIR_NAME)
        .join("workspaces.json"))
}

/// The default data root used when neither the registry file nor an entry
/// in it is usable: `~/.launchpad_studio`, falling back to the current
/// directory if the home directory can't be determined at all.
fn fallback_default_root() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(DEFAULT_DATA_DIR_NAME))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Load the workspace registry from its fixed bootstrap path.
///
/// Distinguishes two failure modes that used to be conflated:
/// - **File absent** — expected on a fresh install. Returns `Ok` with the
///   default single-workspace registry, same as before.
/// - **File present but unparseable** (corrupt, truncated, or an
///   unrecognized shape) — a real error, returned as `Err` rather than
///   silently substituted with a default. This file is never deleted,
///   truncated, or modified by this function or its caller; only surfacing
///   the error lets every mutation route (`create_workspace`,
///   `rename_workspace`, `delete_workspace`, `activate_workspace`,
///   `duplicate_workspace` in `ao-server`) refuse to save — a route that
///   propagates this `Err` via `?` never reaches its `save_registry` call,
///   so it can't overwrite a broken-but-still-recoverable file with a fresh
///   empty one. See [`AoError::WorkspaceRegistryCorrupt`].
///
/// Callers that only need to *resolve* a data root (not mutate the
/// registry) and should keep working even against a corrupt file — namely
/// `data_root::compute_workspace_root_with_provenance` — use
/// [`load_registry_for_resolution`] instead, which wraps this and degrades
/// to the default registry on error without touching the file.
///
/// A registry path that can't be determined at all (home directory
/// unresolvable) is treated like a missing file, not like a corrupt one —
/// there is no file to lose data from in that case.
pub fn load_registry() -> Result<WorkspaceRegistry, AoError> {
    let default_root = fallback_default_root();
    match registry_path() {
        Ok(path) => load_registry_from_path(&path, default_root),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "could not determine workspace registry path; using the default single-workspace registry"
            );
            Ok(WorkspaceRegistry::default_registry(default_root))
        }
    }
}

/// One of the two fixed, verbatim UI-banner sentences carried by
/// [`WorkspaceRegistryDegraded::reason`] when the registry file exists but
/// couldn't even be read (a non-`NotFound` I/O error — permissions, the
/// filesystem going away mid-read, etc). Deliberately a plain sentence with
/// no interpolated detail — see [`WorkspaceRegistryDegraded::detail`] for
/// the underlying error, kept separate so a caller can put it behind a
/// disclosure instead of concatenating it in.
pub const DEGRADED_REASON_UNREADABLE: &str = "Your workspace list could not be read, so \
    Launchpad Studio opened the default workspace instead. Nothing has been changed or deleted.";

/// Same as [`DEGRADED_REASON_UNREADABLE`], for when the file was read
/// successfully but its contents aren't a valid registry (corrupt or
/// truncated JSON).
pub const DEGRADED_REASON_INVALID: &str = "Your workspace list is damaged, so Launchpad Studio \
    opened the default workspace instead. Nothing has been changed or deleted.";

/// Why [`load_registry_for_resolution`] fell back to the default
/// single-workspace registry instead of the real on-disk file, carried
/// alongside the fallback registry so a caller resolving the active data
/// root — [`crate::data_root::compute_workspace_root_with_provenance`], and
/// through it `GET /workspaces/active` — can tell a user their workspace
/// selection didn't silently vanish; the file is untouched and a restart
/// once it's fixed picks the real registry back up.
///
/// Only produced for a registry file that exists but couldn't be used
/// as-is. A genuinely absent file (the normal first-run state) and an
/// unresolvable registry *path* (home directory unresolvable — see
/// [`load_registry`]'s doc comment) both fall back to the same default
/// registry but are NOT degraded: there is no real file being silently
/// discarded in either case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRegistryDegraded {
    /// [`DEGRADED_REASON_UNREADABLE`] or [`DEGRADED_REASON_INVALID`],
    /// verbatim — fixed UI banner copy. Never compose `detail` into this.
    pub reason: String,
    /// The underlying [`AoError::WorkspaceRegistryCorrupt`] message this
    /// degraded state came from.
    pub detail: String,
}

/// Load the registry for data-root resolution only, tolerating a
/// present-but-corrupt file by falling back to the default single-workspace
/// registry instead of propagating [`AoError::WorkspaceRegistryCorrupt`] —
/// paired with [`WorkspaceRegistryDegraded`], set whenever that fallback was
/// actually taken because of a real file rather than a normal first run, so
/// the degradation isn't silent to the end user even though it must stay
/// silent (non-erroring) to every other caller in the process.
///
/// This is safe specifically because resolution is read-only: nothing
/// downstream of it calls [`save_registry`], so a corrupt file can't be
/// clobbered as a side effect of resolving a path from it. The file itself
/// is left exactly as-is; only the in-memory fallback used for this one
/// lookup is synthetic. Do not use this anywhere a caller might go on to
/// save the result — use [`load_registry`] there so a corrupt file still
/// blocks the write.
pub fn load_registry_for_resolution() -> (WorkspaceRegistry, Option<WorkspaceRegistryDegraded>) {
    let default_root = fallback_default_root();
    match registry_path() {
        Ok(path) => load_registry_for_resolution_from_path(&path, default_root),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "could not determine workspace registry path while resolving the active data \
                 root; using the default single-workspace registry"
            );
            (WorkspaceRegistry::default_registry(default_root), None)
        }
    }
}

/// Pure core of [`load_registry_for_resolution`], split out so the
/// garbage-file and missing-file branches can be unit-tested against a temp
/// path instead of the real `~/.launchpad_studio/workspaces.json` — mirrors
/// [`load_registry_from_path`]'s split from [`load_registry`].
fn load_registry_for_resolution_from_path(
    path: &Path,
    default_root: PathBuf,
) -> (WorkspaceRegistry, Option<WorkspaceRegistryDegraded>) {
    match load_registry_from_path_classified(path, default_root.clone()) {
        Ok(registry) => (registry, None),
        Err((reason, err)) => {
            tracing::warn!(
                error = %err,
                "workspace registry could not be loaded while resolving the active data root; \
                 falling back to the default single-workspace registry for this resolution only — \
                 the on-disk file is left untouched and mutation routes still refuse to write while \
                 it remains unparseable"
            );
            (
                WorkspaceRegistry::default_registry(default_root),
                Some(WorkspaceRegistryDegraded {
                    reason: reason.to_string(),
                    detail: err.to_string(),
                }),
            )
        }
    }
}

/// Persist `registry` to its fixed bootstrap path, creating the parent
/// directory if needed. Always a full overwrite — callers are responsible
/// for only calling this with a registry that came from a successful
/// [`load_registry`] (see that function's doc comment for why this is
/// enough to guarantee a corrupt file is never overwritten).
pub fn save_registry(registry: &WorkspaceRegistry) -> Result<(), AoError> {
    save_registry_to_path(&registry_path()?, registry)
}

/// Pure load logic split out of [`load_registry`] so missing-file and
/// corrupt-file handling can be unit-tested against a temp path instead of
/// the real `~/.launchpad_studio/workspaces.json`. Thin wrapper over
/// [`load_registry_from_path_classified`] that discards which of the two
/// failure modes applied — callers that need that distinction (currently
/// only [`load_registry_for_resolution_from_path`]) use that instead.
fn load_registry_from_path(path: &Path, default_root: PathBuf) -> Result<WorkspaceRegistry, AoError> {
    load_registry_from_path_classified(path, default_root).map_err(|(_, err)| err)
}

/// Same as [`load_registry_from_path`], but on failure also returns which of
/// [`DEGRADED_REASON_UNREADABLE`] / [`DEGRADED_REASON_INVALID`] applies —
/// used by [`load_registry_for_resolution_from_path`] to pick the right
/// fixed user-facing sentence without string-matching the formatted
/// [`AoError`] message.
fn load_registry_from_path_classified(
    path: &Path,
    default_root: PathBuf,
) -> Result<WorkspaceRegistry, (&'static str, AoError)> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceRegistry::default_registry(default_root));
        }
        Err(err) => {
            return Err((
                DEGRADED_REASON_UNREADABLE,
                AoError::WorkspaceRegistryCorrupt(format!(
                    "{} could not be read ({err}). The file was left untouched — fix its \
                     permissions or remove it, then retry.",
                    path.display()
                )),
            ));
        }
    };
    serde_json::from_str::<WorkspaceRegistry>(&contents)
        .map(|mut registry| {
            // Backfill `color`/`emoji` on every entry that predates those
            // fields (or was otherwise saved without them) — see
            // `WorkspaceEntry::fill_missing_visual_defaults`.
            for entry in &mut registry.workspaces {
                entry.fill_missing_visual_defaults();
            }
            registry
        })
        .map_err(|err| {
            (
                DEGRADED_REASON_INVALID,
                AoError::WorkspaceRegistryCorrupt(format!(
                    "{} exists but is not a valid workspace registry ({err}). The file was left \
                     untouched — fix or remove it, then retry.",
                    path.display()
                )),
            )
        })
}

/// Pure save logic split out of [`save_registry`] so it can be unit-tested
/// against a temp path — mirrors [`load_registry_from_path`]'s split from
/// [`load_registry`].
fn save_registry_to_path(path: &Path, registry: &WorkspaceRegistry) -> Result<(), AoError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json =
        serde_json::to_string_pretty(registry).map_err(|err| AoError::Json(err.to_string()))?;
    std::fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_points_active_at_default_root() {
        let root = PathBuf::from("/tmp/example-root");
        let registry = WorkspaceRegistry::default_registry(root.clone());
        assert_eq!(registry.active, DEFAULT_WORKSPACE_ID);
        assert_eq!(registry.active_path(), Some(root));
        assert_eq!(registry.workspaces.len(), 1);
    }

    #[test]
    fn active_path_none_when_active_id_unmatched() {
        let registry = WorkspaceRegistry {
            version: 1,
            active: "does-not-exist".to_string(),
            workspaces: vec![WorkspaceEntry {
                id: "other".to_string(),
                name: "Other".to_string(),
                path: PathBuf::from("/tmp/other"),
                color: derive_default_color("other").to_string(),
                emoji: None,
            }],
        };
        assert_eq!(registry.active_path(), None);
    }

    #[test]
    fn active_path_resolves_matching_entry() {
        let registry = WorkspaceRegistry {
            version: 1,
            active: "work".to_string(),
            workspaces: vec![
                WorkspaceEntry {
                    id: "default".to_string(),
                    name: "Default".to_string(),
                    path: PathBuf::from("/tmp/default"),
                    color: derive_default_color("default").to_string(),
                    emoji: None,
                },
                WorkspaceEntry {
                    id: "work".to_string(),
                    name: "Work".to_string(),
                    path: PathBuf::from("/tmp/work"),
                    color: derive_default_color("work").to_string(),
                    emoji: None,
                },
            ],
        };
        assert_eq!(registry.active_path(), Some(PathBuf::from("/tmp/work")));
    }

    // (a) absent file → default registry, no error.
    #[test]
    fn load_registry_from_path_missing_file_falls_back_to_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist.json");
        let default_root = PathBuf::from("/tmp/fallback-root");

        let registry = load_registry_from_path(&missing, default_root.clone())
            .expect("a missing file must not be treated as an error");
        assert_eq!(registry, WorkspaceRegistry::default_registry(default_root));
    }

    // (b) file containing garbage → error surfaced, naming the path, and the
    // bytes on disk are left exactly as they were (no silent default written
    // over them).
    #[test]
    fn load_registry_from_path_garbage_file_errors_and_leaves_bytes_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        let garbage = b"{ not valid json at all }}}";
        std::fs::write(&path, garbage).expect("write garbage file");
        let default_root = PathBuf::from("/tmp/fallback-root");

        let err = load_registry_from_path(&path, default_root)
            .expect_err("garbage content must not silently become the default registry");
        let message = err.to_string();
        assert!(
            message.contains(&path.display().to_string()),
            "error must name the file path so a user can find it, got: {message}"
        );

        assert_eq!(
            std::fs::read(&path).unwrap(),
            garbage,
            "a failed load must never modify, truncate, or delete the file it read"
        );
    }

    // (c) file containing a truncated-but-otherwise-valid JSON prefix →
    // same error/refusal treatment as outright garbage — no special-casing
    // needed since serde_json already fails a truncated document.
    #[test]
    fn load_registry_from_path_truncated_file_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        let full = WorkspaceRegistry {
            version: 1,
            active: "work".to_string(),
            workspaces: vec![WorkspaceEntry {
                id: "work".to_string(),
                name: "Work".to_string(),
                path: PathBuf::from("/tmp/work-root"),
                color: derive_default_color("work").to_string(),
                emoji: None,
            }],
        };
        let full_json = serde_json::to_string(&full).unwrap();
        // Cut the JSON off partway through — a plausible shape for a crash
        // or a disk-full write to leave behind.
        let truncated = &full_json[..full_json.len() / 2];
        std::fs::write(&path, truncated).expect("write truncated file");
        let default_root = PathBuf::from("/tmp/fallback-root");

        let err = load_registry_from_path(&path, default_root)
            .expect_err("a truncated JSON prefix must not parse as a valid registry");
        assert!(matches!(err, AoError::WorkspaceRegistryCorrupt(_)));
    }

    #[test]
    fn load_registry_from_path_valid_file_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        let written = WorkspaceRegistry {
            version: 1,
            active: "work".to_string(),
            workspaces: vec![WorkspaceEntry {
                id: "work".to_string(),
                name: "Work".to_string(),
                path: PathBuf::from("/tmp/work-root"),
                color: derive_default_color("work").to_string(),
                emoji: None,
            }],
        };
        std::fs::write(&path, serde_json::to_string(&written).unwrap()).unwrap();

        let loaded = load_registry_from_path(&path, PathBuf::from("/tmp/unused")).unwrap();
        assert_eq!(loaded, written);
    }

    #[test]
    fn save_and_load_round_trip_via_registry_path_helper() {
        // save_registry/registry_path are exercised indirectly here by
        // going through the same file-based path load_registry_from_path
        // uses, since registry_path() itself is pinned to the real home
        // directory and shouldn't be redirected in a unit test.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        let registry = WorkspaceRegistry::default_registry(PathBuf::from("/tmp/root"));
        let json = serde_json::to_string_pretty(&registry).unwrap();
        std::fs::write(&path, json).unwrap();

        let loaded = load_registry_from_path(&path, PathBuf::from("/tmp/unused")).unwrap();
        assert_eq!(loaded, registry);
    }

    // (b)+(d) A mutation route's exact shape — load, then only on success
    // mutate and save — refuses to write when the load fails, and the
    // refusal leaves the original bytes on disk byte-for-byte unchanged.
    // There is deliberately no way to reach `save_registry_to_path` here
    // from an `Err`: the `?` below returns out of this helper before a
    // `WorkspaceRegistry` value ever exists to save.
    #[test]
    fn corrupt_registry_load_refuses_the_subsequent_write_and_preserves_bytes() {
        fn simulate_mutation_route(path: &Path) -> Result<(), AoError> {
            let mut registry = load_registry_from_path(path, PathBuf::from("/tmp/fallback-root"))?;
            registry.active = "changed".to_string();
            save_registry_to_path(path, &registry)
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        let garbage = b"not json at all {{{";
        std::fs::write(&path, garbage).expect("write garbage file");

        let result = simulate_mutation_route(&path);
        assert!(result.is_err(), "the write must be refused, not attempted");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            garbage,
            "a refused write must never touch the original file"
        );
    }

    // ── `load_registry_for_resolution_from_path` degraded-state tests ──────

    #[test]
    fn resolution_reports_degraded_for_a_garbage_registry_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        std::fs::write(&path, b"{ not valid json at all }}}").expect("write garbage file");
        let default_root = PathBuf::from("/tmp/fallback-root");

        let (registry, degraded) =
            load_registry_for_resolution_from_path(&path, default_root.clone());

        // Resolution still produces a usable (synthetic default) registry —
        // it must never surface an `Err` to a caller that only wants to
        // resolve a path.
        assert_eq!(registry, WorkspaceRegistry::default_registry(default_root));

        let degraded = degraded.expect("a garbage file must report degraded resolution");
        assert_eq!(degraded.reason, DEGRADED_REASON_INVALID);
        assert!(
            degraded.detail.contains(&path.display().to_string()),
            "detail must name the file so a user/UI disclosure can locate it, got: {}",
            degraded.detail
        );
    }

    #[test]
    fn resolution_does_not_report_degraded_for_a_missing_registry_file() {
        // A missing file is the normal first-run state and must stay
        // silent — this is the case that must NOT regress into a false
        // "degraded" banner on every fresh install.
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist.json");
        let default_root = PathBuf::from("/tmp/fallback-root");

        let (registry, degraded) =
            load_registry_for_resolution_from_path(&missing, default_root.clone());

        assert_eq!(registry, WorkspaceRegistry::default_registry(default_root));
        assert_eq!(
            degraded, None,
            "an absent registry file must not be reported as degraded"
        );
    }

    #[test]
    fn resolution_does_not_report_degraded_for_a_valid_registry_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        let written = registry_with(vec![entry("work", "/tmp/work-root")]);
        std::fs::write(&path, serde_json::to_string(&written).unwrap()).unwrap();

        let (registry, degraded) =
            load_registry_for_resolution_from_path(&path, PathBuf::from("/tmp/unused"));

        assert_eq!(registry, written);
        assert_eq!(degraded, None);
    }

    /// Test-only helpers mirroring `routes::workspaces`'s own `entry` /
    /// `registry_with` fixtures, kept local since this module can't depend
    /// on `ao-server`.
    fn entry(id: &str, path: &str) -> WorkspaceEntry {
        WorkspaceEntry {
            id: id.to_string(),
            name: id.to_string(),
            path: PathBuf::from(path),
            color: derive_default_color(id).to_string(),
            emoji: None,
        }
    }

    fn registry_with(entries: Vec<WorkspaceEntry>) -> WorkspaceRegistry {
        WorkspaceRegistry {
            version: 1,
            active: entries.first().map(|e| e.id.clone()).unwrap_or_default(),
            workspaces: entries,
        }
    }

    // ── `color`/`emoji` backward compatibility & derivation ─────────────

    /// The exact on-disk shape the real `~/.launchpad_studio/workspaces.json`
    /// registry has for any user who created workspaces before `color`/
    /// `emoji` existed: entries with only `id`/`name`/`path`. This MUST
    /// still parse — a non-defaulted field here would make
    /// `AoError::WorkspaceRegistryCorrupt` fatal for every mutation route on
    /// every such registry. See `empty_color_default` (for `color`) and
    /// [`WorkspaceEntry::emoji`]'s doc comment (for why `emoji` needs no
    /// sentinel of its own — `Option<T>`'s own default is already `None`).
    #[test]
    fn legacy_three_field_registry_json_still_deserializes() {
        let legacy_json = r#"{
            "version": 1,
            "active": "default",
            "workspaces": [
                { "id": "default", "name": "Default", "path": "/tmp/legacy-root" },
                { "id": "work", "name": "Work", "path": "/tmp/legacy-work" }
            ]
        }"#;

        let registry: WorkspaceRegistry =
            serde_json::from_str(legacy_json).expect("a legacy 3-field entry must deserialize");
        assert_eq!(registry.workspaces.len(), 2);

        // Straight `serde_json::from_str` (not `load_registry_from_path`)
        // exercises only the field-level `#[serde(default)]` behavior:
        // `color` lands as the empty-string sentinel (backfilled one layer
        // up, in `load_registry_from_path_classified`, exercised below);
        // `emoji` lands directly as `None` — its final, permanent value,
        // never backfilled at all.
        for entry in &registry.workspaces {
            assert_eq!(entry.color, "");
            assert_eq!(entry.emoji, None);
        }
    }

    /// The same legacy JSON, but loaded through
    /// `load_registry_from_path` — the path every real caller actually
    /// uses — asserts `color`'s empty sentinel gets backfilled with a real,
    /// id-derived value, while `emoji` stays `None` rather than being
    /// defaulted to anything. Also satisfies the "registry JSON omitting
    /// `emoji` deserializes to `None`, and `fill_missing_visual_defaults`
    /// leaves it `None`" case.
    #[test]
    fn load_registry_from_path_backfills_color_but_leaves_emoji_none_for_legacy_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("workspaces.json");
        let legacy_json = r#"{
            "version": 1,
            "active": "default",
            "workspaces": [
                { "id": "default", "name": "Default", "path": "/tmp/legacy-root" },
                { "id": "work", "name": "Work", "path": "/tmp/legacy-work" }
            ]
        }"#;
        std::fs::write(&path, legacy_json).expect("write legacy registry file");

        let registry =
            load_registry_from_path(&path, PathBuf::from("/tmp/unused")).expect("must load");

        for entry in &registry.workspaces {
            assert!(!entry.color.is_empty(), "color must be backfilled");
            assert_eq!(entry.color, derive_default_color(&entry.id));
            assert_eq!(
                entry.emoji, None,
                "emoji must stay unset — the user hasn't chosen one, and defaulting it here \
                 would make 'no emoji' unrepresentable again"
            );
        }
    }

    /// Dedicated, minimal version of the same "omit `emoji` → stays `None`
    /// through both deserialize and `fill_missing_visual_defaults`" claim,
    /// isolated from the full `load_registry_from_path` pipeline above.
    #[test]
    fn legacy_registry_missing_emoji_deserializes_to_none_and_fill_missing_visual_defaults_leaves_it_none()
    {
        let legacy_json = r#"{
            "version": 1,
            "active": "default",
            "workspaces": [
                { "id": "default", "name": "Default", "path": "/tmp/legacy-root" }
            ]
        }"#;
        let mut registry: WorkspaceRegistry =
            serde_json::from_str(legacy_json).expect("must deserialize without an emoji key");
        assert_eq!(registry.workspaces[0].emoji, None);

        registry.workspaces[0].fill_missing_visual_defaults();

        assert_eq!(
            registry.workspaces[0].emoji, None,
            "fill_missing_visual_defaults must not manufacture an emoji"
        );
        assert!(
            !registry.workspaces[0].color.is_empty(),
            "color must still be backfilled"
        );
    }

    /// A `WorkspaceEntry` with an unset `emoji` must serialize with the
    /// `"emoji"` key omitted entirely — never as `"emoji": null`. See
    /// [`WorkspaceEntry::emoji`]'s doc comment for why a `null` would be
    /// unsafe for the second worktree/app sharing this registry file.
    #[test]
    fn workspace_entry_with_none_emoji_omits_the_emoji_key_from_json() {
        let entry = WorkspaceEntry {
            id: "work".to_string(),
            name: "Work".to_string(),
            path: PathBuf::from("/tmp/work-root"),
            color: derive_default_color("work").to_string(),
            emoji: None,
        };

        let json = serde_json::to_string(&entry).expect("serialize");

        assert!(
            !json.contains("\"emoji\""),
            "an unset emoji must be omitted from the JSON entirely, not serialized as \
             \"emoji\":null — got: {json}"
        );
    }

    #[test]
    fn derive_default_color_is_stable_across_calls() {
        let a = derive_default_color("some-workspace-id");
        let b = derive_default_color("some-workspace-id");
        assert_eq!(a, b, "the same id must always derive the same color");
        assert!(WORKSPACE_COLOR_PALETTE.contains(&a));
    }

    #[test]
    fn derive_default_color_varies_across_distinct_ids() {
        // Not a strict requirement (a collision is technically possible with
        // only 8 buckets), but with these specific ids it demonstrates the
        // derivation isn't a constant function that always returns the same
        // palette entry regardless of input.
        let colors: std::collections::HashSet<&str> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .iter()
            .map(|id| derive_default_color(id))
            .collect();
        assert!(
            colors.len() > 1,
            "distinct ids should not all collide onto the same color"
        );
    }
}
