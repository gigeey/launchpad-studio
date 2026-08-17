use std::path::{Path, PathBuf};

use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use ao_persistence::paths::DataRoot;
use ao_persistence::PersistenceLayer;
use ao_protocol::data_root::{
    resolve_data_root_or_cwd, resolve_data_root_with_provenance, startup_fallback,
    user_pinned_data_root_value, RootProvenance, StartupFallback, DATA_DIR_ENV_VAR,
};
use ao_protocol::error::AoError;
use ao_protocol::workspaces::{
    derive_default_color, load_registry, save_registry, CreateWorkspaceResponse, WorkspaceEntry,
    WorkspaceRegistry, WorkspaceRegistryDegraded,
};

use crate::error::AppError;
use crate::workspace_lock;

// ── Validation helpers ──────────────────────────────────────────────────────

/// Trim and validate a workspace name, returning the trimmed value.
fn require_name(name: &str) -> Result<String, AppError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(AppError(AoError::ValidationError(
            "workspace name must not be empty".to_string(),
        )));
    }
    Ok(trimmed.to_string())
}

/// Reject a relative workspace path. Activation just points the data-root
/// resolver at a fixed directory, so the path has to be unambiguous
/// regardless of the server process's current working directory.
fn require_absolute_path(path: &Path) -> Result<(), AppError> {
    if !path.is_absolute() {
        return Err(AppError(AoError::ValidationError(format!(
            "workspace path must be absolute: {}",
            path.display()
        ))));
    }
    Ok(())
}

/// Lexically collapse `.` components and a trailing separator, without
/// touching the filesystem. Used only to compare two paths for equality even
/// when one or both don't exist yet — `Path::canonicalize` requires the path
/// to exist, so it isn't an option here.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        if !matches!(component, std::path::Component::CurDir) {
            result.push(component.as_os_str());
        }
    }
    result
}

/// Reject a path that collides with an already-registered workspace.
fn require_no_path_collision(registry: &WorkspaceRegistry, path: &Path) -> Result<(), AppError> {
    let normalized = normalize_lexical(path);
    let collides = registry
        .workspaces
        .iter()
        .any(|entry| normalize_lexical(&entry.path) == normalized);
    if collides {
        return Err(AppError(AoError::ValidationError(format!(
            "a workspace is already registered at {}",
            path.display()
        ))));
    }
    Ok(())
}

/// Reject a path that already exists as a non-empty directory, or as
/// something other than a directory (a file, etc). A missing path or an
/// existing-but-empty directory are both fine to build a workspace tree in.
///
/// Used only by [`duplicate_workspace`] — copying a workspace's tree onto an
/// existing Launchpad data root would silently merge two workspaces' files
/// together, so unlike [`create_workspace`] there is no adopt outcome here,
/// only create-or-reject. [`create_workspace`] has its own richer
/// classification, [`classify_create_target`], which this predates.
async fn require_path_creatable(path: &Path) -> Result<(), AppError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error_to_app_error(e)),
    };
    if !metadata.is_dir() {
        return Err(AppError(AoError::ValidationError(format!(
            "workspace path already exists and is not a directory: {}",
            path.display()
        ))));
    }
    let mut entries = tokio::fs::read_dir(path)
        .await
        .map_err(io_error_to_app_error)?;
    let has_entry = entries
        .next_entry()
        .await
        .map_err(io_error_to_app_error)?
        .is_some();
    if has_entry {
        return Err(AppError(AoError::ValidationError(format!(
            "workspace path already exists and is not empty: {}",
            path.display()
        ))));
    }
    Ok(())
}

/// Rejection message for [`classify_create_target`] when `path` exists,
/// isn't empty, and isn't a valid Launchpad data root either. Pulled out to
/// a constant so the handler and its tests can't drift apart on wording —
/// the frontend matches against this exact string.
const NOT_ADOPTABLE_MESSAGE: &str = "That folder already has contents and isn't a Launchpad \
     workspace. Choose an empty folder, or a folder that already contains a Launchpad workspace.";

/// The three non-collision outcomes [`create_workspace`] can classify its
/// target path into (the fourth possible outcome — the path colliding with
/// an already-registered workspace — is handled earlier, by
/// [`require_no_path_collision`]).
#[derive(Debug)]
enum CreateTarget {
    /// Path is missing, or exists as an empty directory — safe to scaffold
    /// via `DataRoot::ensure_directories`.
    Creatable,
    /// Path exists, is non-empty, and already looks like a valid Launchpad
    /// data root ([`DataRoot::looks_like_data_root`]) — adopt it instead of
    /// scaffolding over it.
    ExistingDataRoot,
}

/// Classify `path` for [`create_workspace`], or reject it outright with
/// [`NOT_ADOPTABLE_MESSAGE`] if it's non-empty and not a valid data root
/// (including the degenerate case where it exists but isn't a directory at
/// all). Read-only: never creates, deletes, or modifies anything on disk.
async fn classify_create_target(path: &Path) -> Result<CreateTarget, AppError> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CreateTarget::Creatable),
        Err(e) => return Err(io_error_to_app_error(e)),
    };

    if metadata.is_dir() {
        let mut entries = tokio::fs::read_dir(path)
            .await
            .map_err(io_error_to_app_error)?;
        let has_entry = entries
            .next_entry()
            .await
            .map_err(io_error_to_app_error)?
            .is_some();
        if !has_entry {
            return Ok(CreateTarget::Creatable);
        }
        if DataRoot::new(path).looks_like_data_root().await {
            return Ok(CreateTarget::ExistingDataRoot);
        }
    }

    Err(AppError(AoError::ValidationError(
        NOT_ADOPTABLE_MESSAGE.to_string(),
    )))
}

/// Map a filesystem error to the right HTTP status: a permission failure is
/// the caller's fault (400), anything else is ours (500).
fn io_error_to_app_error(e: std::io::Error) -> AppError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => AppError(AoError::ValidationError(format!(
            "workspace path is not writable: {e}"
        ))),
        _ => AppError(AoError::Internal(format!(
            "workspace filesystem operation failed: {e}"
        ))),
    }
}

/// Map the [`AoError`] bubbling out of [`DataRoot::ensure_directories`] or
/// [`copy_workspace_tree`] (both of which only ever produce `AoError::Io`)
/// through the same permission-aware split as [`io_error_to_app_error`].
fn map_directory_creation_error(e: AoError) -> AppError {
    if let AoError::Io(io_err) = &e {
        if io_err.kind() == std::io::ErrorKind::PermissionDenied {
            return AppError(AoError::ValidationError(format!(
                "workspace path is not writable: {io_err}"
            )));
        }
    }
    AppError(e)
}

fn workspace_not_found(id: &str) -> AppError {
    AppError(AoError::WorkspaceNotFound(format!(
        "workspace '{id}' not found"
    )))
}

/// Refuse a workspace-registry mutation when this process's active data
/// root is pinned via an INHERITED `LAUNCHPAD_STUDIO_DATA_DIR` — the server-side
/// enforcement of the same rule the frontend already applies client-side by
/// disabling the workspace switcher whenever `provenance === "env_override"`
/// (see `WorkspaceIndicator.tsx` / `SettingsView.tsx`). The two worktree
/// tracks share ONE on-disk registry file regardless of which data root
/// each is pinned to (`registry_path` is deliberately outside any data
/// root — see its doc comment), so without this guard a pinned track's
/// mutation route would silently rewrite the shared file out from under the
/// unpinned track. `workspace_lock` doesn't help here: it only guards two
/// processes activating the same TARGET data root, not two processes
/// writing the same registry FILE.
///
/// EXEMPTS a pin this process set on ITSELF during startup crash recovery
/// ([`ao_protocol::data_root::pin_startup_fallback_root`]), which is why
/// this asks [`user_pinned_data_root_value`] rather than "is the env var
/// set". A degraded boot pins the default data root so the app can come up
/// at all, and the workspace registry is the only route back out of it —
/// refusing mutations there would trade a crash loop for a locked room, in
/// which the switcher is dead, every mutation 400s, and the user's real
/// workspace is unreachable from inside the app. The rationale above is
/// about not letting one deliberately-pinned track rewrite another's shared
/// state; a process that pinned itself has no such second track to protect,
/// and no operator intent to honor.
///
/// Activating OUT of fallback mode is safe by construction because
/// [`probe_target_data_root`] pre-flights the target root before the
/// pointer is committed: the exemption here and that probe complete each
/// other — this one makes the escape reachable, the probe makes it
/// survivable.
///
/// Checks the env var directly (via [`user_pinned_data_root_value`], which
/// reads it live) rather than calling [`resolve_data_root_with_provenance`]
/// and matching on `provenance == RootProvenance::EnvOverride` — the two
/// are provably equivalent (that function's env-var branch IS this exact
/// check, down to the same fallback exemption, and always wins
/// unconditionally when it applies), but going through it here would mean
/// occasionally touching `WORKSPACE_ROOT_CACHE`, a process-wide cache with
/// no test-visible reset. See [`user_pinned_data_root_value`]'s doc comment
/// for the full reasoning.
///
/// Called first, before any registry read in each of the five mutation
/// handlers below, so a refusal never reaches `load_registry`/
/// `save_registry` and never touches the registry file on disk.
fn require_data_root_not_pinned() -> Result<(), AppError> {
    let Some(value) = user_pinned_data_root_value() else {
        return Ok(());
    };
    Err(AppError(AoError::WorkspaceMutationBlockedByPinnedDataRoot {
        env_var: DATA_DIR_ENV_VAR.to_string(),
        value,
    }))
}

// ── Directory tree copy (for duplicate) ─────────────────────────────────────

/// Recursively copy `src` to `dst`, off the async runtime's worker thread
/// since it's all blocking filesystem calls.
async fn copy_workspace_tree(src: PathBuf, dst: PathBuf) -> Result<(), AoError> {
    tokio::task::spawn_blocking(move || copy_dir_recursive_sync(&src, &dst))
        .await
        .map_err(|e| AoError::Internal(format!("join error while duplicating workspace: {e}")))?
        .map_err(AoError::Io)
}

fn copy_dir_recursive_sync(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive_sync(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// ── DTOs ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct WorkspaceListResponse {
    pub workspaces: Vec<WorkspaceEntry>,
    pub active: String,
}

#[derive(Debug, Serialize)]
pub struct ActiveWorkspaceResponse {
    pub path: PathBuf,
    pub provenance: RootProvenance,
    /// The registered workspace's display name, populated ONLY when
    /// `provenance` is [`RootProvenance::Registry`]. Under the env-override
    /// or home-default branches there is no registry entry backing the
    /// resolved path, so this is `null` rather than a guessed or
    /// last-known name — see [`get_active_workspace`] for why.
    pub name: Option<String>,
    /// Populated when the on-disk workspace registry exists but couldn't be
    /// read or parsed, in which case `path`/`provenance` above already
    /// reflect the default-workspace fallback this response still returns
    /// 200 with. `null` in every other case, including the normal
    /// first-run state where no registry file exists yet. See
    /// [`WorkspaceRegistryDegraded`].
    pub degraded: Option<WorkspaceRegistryDegraded>,
    /// Populated when this process's `AppState` couldn't be initialized
    /// against `path` above and startup fell back to the default data root
    /// instead of crashing — see [`StartupFallback`]. `null` in the normal
    /// case. Unlike `degraded` above (a corrupt *registry file*), this can
    /// fire regardless of which `provenance` branch resolved `path`, since
    /// it reports a failure to initialize the resolved root itself, not a
    /// failure to read the registry that helped choose it.
    pub startup_fallback: Option<StartupFallback>,
}

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub path: PathBuf,
    /// Optional hex color from `WORKSPACE_COLOR_PALETTE`. Absent (or
    /// `null`) means: derive one deterministically from the new entry's
    /// generated id, via [`derive_default_color`].
    #[serde(default)]
    pub color: Option<String>,
    /// Optional emoji. Absent (or `null`) means no emoji is set on the
    /// created entry — `WorkspaceEntry::emoji` is itself optional, so this
    /// is passed straight through rather than defaulted to a placeholder.
    #[serde(default)]
    pub emoji: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RenameWorkspaceRequest {
    pub name: String,
    /// Optional hex color to update to. Absent (or `null`) leaves the
    /// entry's existing `color` unchanged — this field is a rename request,
    /// not a full replace, so omission must never reset it to a default.
    #[serde(default)]
    pub color: Option<String>,
    /// Optional emoji update, three-state — a plain `Option<String>` can't
    /// express this, since a `null` and an absent field would both
    /// collapse to `None`:
    /// - field absent from the request body → outer `None` → leave the
    ///   entry's existing emoji unchanged.
    /// - explicit JSON `null` → `Some(None)` → clear a previously-set
    ///   emoji (representable now that `WorkspaceEntry::emoji` is itself
    ///   `Option<String>` — see that field's doc comment).
    /// - a string → `Some(Some(value))` → set the emoji to that value.
    #[serde(default, deserialize_with = "double_option")]
    pub emoji: Option<Option<String>>,
}

/// Serde helper distinguishing "field absent" (`None`) from "field present
/// with an explicit `null`" (`Some(None)`) from "field present with a
/// value" (`Some(Some(value))`) — the standard two-`Option` trick, needed
/// because a plain `#[serde(default)] Option<T>` collapses the first two
/// into the same `None`. Used by [`RenameWorkspaceRequest::emoji`] to make
/// "clear this field" distinguishable from "don't touch this field" now
/// that the value it targets ([`WorkspaceEntry::emoji`]) is itself
/// optional.
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
pub struct DuplicateWorkspaceRequest {
    pub name: String,
    pub path: PathBuf,
}

// ── GET /workspaces ──────────────────────────────────────────────────────────

/// GET /workspaces — list every registered workspace and which one is active.
///
/// Errors (rather than silently returning an empty list) if the registry
/// file exists but failed to parse — see [`load_registry`].
pub async fn list_workspaces() -> Result<Json<WorkspaceListResponse>, AppError> {
    let registry = load_registry()?;
    Ok(Json(WorkspaceListResponse {
        workspaces: registry.workspaces,
        active: registry.active,
    }))
}

// ── GET /workspaces/active ───────────────────────────────────────────────────

/// GET /workspaces/active — the data root this process is ACTUALLY running
/// against, per [`resolve_data_root_with_provenance`], plus which precedence
/// branch produced it and (only for the registry branch) that workspace's
/// display name.
///
/// This exists so a client can never disagree with the resolver. Reading
/// `GET /workspaces` and assuming its `active` entry is what's actually in
/// effect is unsound once `LAUNCHPAD_STUDIO_DATA_DIR` is set — that branch
/// outranks the registry entirely and is never cross-checked against it, so
/// the registry's `active` pointer can (and, under the dual-worktree
/// contract, always does) name a workspace this process isn't running on.
///
/// The workspace name is looked up by matching the resolved path against
/// the registry's entries (not by re-reading the registry's current
/// `active` id) so a `POST /workspaces/{id}/activate` call that hasn't been
/// picked up by a process restart yet can't produce a name/path mismatch —
/// activation only updates the pointer, it doesn't take effect until
/// restart, but the registry file itself changes immediately.
///
/// Always returns 200, even when the registry is corrupt: resolution itself
/// never errors on a bad registry file (see
/// [`resolve_data_root_with_provenance`]), and `provenance` is guaranteed
/// to NOT be [`RootProvenance::Registry`] whenever `degraded` is populated
/// (see `compute_workspace_root_with_provenance`'s doc comment), so the
/// `load_registry()?` name lookup below is never reached against the same
/// file that just failed to resolve — it would only fail identically and
/// turn this into an error response, which the app boot path can't afford.
pub async fn get_active_workspace() -> Result<Json<ActiveWorkspaceResponse>, AppError> {
    let (path, provenance, degraded) = resolve_data_root_with_provenance()?;

    let registry_workspaces = if provenance == RootProvenance::Registry {
        load_registry()?.workspaces
    } else {
        Vec::new()
    };

    Ok(Json(build_active_workspace_response(
        path,
        provenance,
        degraded,
        &registry_workspaces,
        startup_fallback(),
    )))
}

/// Pure core of [`get_active_workspace`]: builds the response from an
/// already-resolved `(path, provenance, degraded)` triple, whichever
/// registry entries are on hand for the name lookup, and the process-wide
/// startup-fallback record (if any). Split out so the degraded-state and
/// name-lookup branches are unit-testable with synthetic values —
/// `resolve_data_root_with_provenance`'s registry branch is pinned to the
/// real `$HOME` (see this module's test-header comment) and must never be
/// pointed at a temp file that isn't the user's real registry, so there is
/// no safe way to exercise `get_active_workspace` itself end to end with a
/// corrupt file in a test.
fn build_active_workspace_response(
    path: PathBuf,
    provenance: RootProvenance,
    degraded: Option<WorkspaceRegistryDegraded>,
    registry_workspaces: &[WorkspaceEntry],
    startup_fallback: Option<StartupFallback>,
) -> ActiveWorkspaceResponse {
    let name = if provenance == RootProvenance::Registry {
        registry_workspaces
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.name.clone())
    } else {
        None
    };

    ActiveWorkspaceResponse {
        path,
        provenance,
        name,
        degraded,
        startup_fallback,
    }
}

// ── POST /workspaces ─────────────────────────────────────────────────────────

/// POST /workspaces — register a workspace, creating OR adopting depending
/// on what's already at `path`:
///   - missing, or an existing empty directory: CREATE — builds the on-disk
///     directory tree via [`DataRoot::ensure_directories`] as before.
///   - existing, non-empty, and already a valid Launchpad data root: ADOPT —
///     registers the pointer as-is; nothing on disk is touched.
///   - existing, non-empty, and not a valid data root: rejected with
///     [`NOT_ADOPTABLE_MESSAGE`].
///   - already registered at this path: rejected, unchanged from before
///     (see [`require_no_path_collision`]).
///
/// Never activates the result — switching data roots only happens via
/// `POST /workspaces/{id}/activate` followed by an app restart.
pub async fn create_workspace(
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<CreateWorkspaceResponse>), AppError> {
    require_data_root_not_pinned()?;
    let name = require_name(&req.name)?;
    require_absolute_path(&req.path)?;

    let mut registry = load_registry()?;
    require_no_path_collision(&registry, &req.path)?;

    let adopted = match classify_create_target(&req.path).await? {
        CreateTarget::Creatable => {
            DataRoot::new(req.path.clone())
                .ensure_directories()
                .await
                .map_err(map_directory_creation_error)?;
            false
        }
        // Adopt: register the existing root as-is. Deliberately no
        // filesystem write of any kind on this branch — an already-valid
        // data root must come out of this handler byte-for-byte unchanged.
        CreateTarget::ExistingDataRoot => true,
    };

    let id = Uuid::new_v4().to_string();
    // Absent color means "use the default", derived deterministically from
    // the freshly-generated `id` (so it's stable from the moment this
    // entry is created onward, not recomputed differently on every future
    // load). Absent emoji means exactly what it says — no emoji chosen —
    // and is passed straight through: unlike color there is no default to
    // fall back to, see `WorkspaceEntry::emoji`.
    let color = req.color.unwrap_or_else(|| derive_default_color(&id).to_string());
    let entry = WorkspaceEntry {
        id,
        name,
        path: req.path,
        color,
        emoji: req.emoji,
    };
    registry.workspaces.push(entry.clone());
    save_registry(&registry)?;

    Ok((
        StatusCode::CREATED,
        Json(CreateWorkspaceResponse {
            workspace: entry,
            adopted,
        }),
    ))
}

// ── PATCH /workspaces/{id} ───────────────────────────────────────────────────

/// PATCH /workspaces/{id} — rename a workspace, and optionally update its
/// `color`/`emoji`. The path is immutable; to relocate a workspace's data,
/// duplicate it at the new path and deregister the old entry.
///
/// `color` is genuinely optional in [`RenameWorkspaceRequest`]: an absent
/// field leaves the entry's existing value untouched rather than resetting
/// it to a default — this is a partial update, not a replace, so a client
/// renaming a workspace without knowing/caring about its color must never
/// accidentally blow that color away.
///
/// `emoji` is three-state (see [`RenameWorkspaceRequest::emoji`]): absent
/// leaves it unchanged, an explicit JSON `null` clears it, and a string
/// sets it.
pub async fn rename_workspace(
    AxumPath(id): AxumPath<String>,
    Json(req): Json<RenameWorkspaceRequest>,
) -> Result<Json<WorkspaceEntry>, AppError> {
    require_data_root_not_pinned()?;
    let name = require_name(&req.name)?;

    let mut registry = load_registry()?;
    let entry = registry
        .workspaces
        .iter_mut()
        .find(|e| e.id == id)
        .ok_or_else(|| workspace_not_found(&id))?;
    entry.name = name;
    if let Some(color) = req.color {
        entry.color = color;
    }
    if let Some(new_emoji) = req.emoji {
        entry.emoji = new_emoji;
    }
    let updated = entry.clone();
    save_registry(&registry)?;

    Ok(Json(updated))
}

// ── DELETE /workspaces/{id} ──────────────────────────────────────────────────

/// DELETE /workspaces/{id} — deregister a workspace. The workspace's files on
/// disk are left untouched; this only removes the registry entry. Rejects
/// deleting the currently-active workspace and rejects deleting the last
/// remaining workspace, since either would leave the registry unable to
/// resolve a data root.
pub async fn delete_workspace(AxumPath(id): AxumPath<String>) -> Result<StatusCode, AppError> {
    require_data_root_not_pinned()?;
    let mut registry = load_registry()?;

    if !registry.workspaces.iter().any(|e| e.id == id) {
        return Err(workspace_not_found(&id));
    }
    if registry.active == id {
        return Err(AppError(AoError::Conflict(
            "cannot delete the active workspace; activate a different workspace first"
                .to_string(),
        )));
    }
    if registry.workspaces.len() == 1 {
        return Err(AppError(AoError::Conflict(
            "cannot delete the last remaining workspace".to_string(),
        )));
    }

    registry.workspaces.retain(|e| e.id != id);
    save_registry(&registry)?;

    Ok(StatusCode::NO_CONTENT)
}

// ── POST /workspaces/{id}/activate ───────────────────────────────────────────

/// POST /workspaces/{id}/activate — point the registry's `active` pointer at
/// `id` and persist it.
///
/// This ONLY writes the pointer file. It deliberately does not swap the
/// running process's data root — persistence, the event bus, and every
/// in-memory store were all wired up against the data root resolved at
/// process start, so an in-place swap can't be done safely here. The client
/// is responsible for triggering a full app restart afterward; only then
/// does the new active workspace take effect.
///
/// Refuses with 409 if the TARGET workspace's root ([`workspace_lock`]) is
/// currently held by a different, still-live ao-server process — switching
/// this registry pointer without that check would let a restart point two
/// processes at the same data root. A stale lock (dead pid) never blocks
/// this. See `ao_server::workspace_lock` for why the guard has no effect
/// until whichever process is holding a root open restarts.
///
/// Refuses with 400 ([`AoError::WorkspaceActivationTargetUnopenable`]) if
/// [`probe_target_data_root`] can't open the target root. Without this, a
/// missing/empty/unopenable target would still get its pointer persisted
/// here, the app would crash on the restart this activation triggers, and
/// every subsequent launch would crash identically with no in-app way
/// back — see that function's doc comment for the full rationale. The
/// probe runs AFTER the pinned-data-root and lock guards but BEFORE the
/// mutation below, so a failure leaves the registry byte-identical to
/// before the request.
pub async fn activate_workspace(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<WorkspaceListResponse>, AppError> {
    require_data_root_not_pinned()?;
    let current_root = resolve_data_root_or_cwd();
    activate_workspace_with_current_root(id, &current_root).await
}

/// Core of [`activate_workspace`], parameterized on this process's own
/// resolved data root rather than resolving it itself, so it can be driven
/// directly in tests against a synthetic `current_root`. Going through
/// [`resolve_data_root_or_cwd`] touches a process-wide cache that latches
/// onto whichever registry file is live the first time anything in the
/// process calls it (see that function's doc comment) — the real
/// `~/.launchpad_studio/workspaces.json` must never be that in a test, so
/// [`activate_workspace`] (the only non-test caller) is the one place that
/// resolves the real value.
async fn activate_workspace_with_current_root(
    id: String,
    current_root: &Path,
) -> Result<Json<WorkspaceListResponse>, AppError> {
    let mut registry = load_registry()?;
    let target = registry
        .workspaces
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(|| workspace_not_found(&id))?;
    workspace_lock::require_not_locked(&target.path).await?;
    probe_target_data_root(&target.path, current_root).await?;

    registry.active = id;
    save_registry(&registry)?;

    Ok(Json(WorkspaceListResponse {
        workspaces: registry.workspaces,
        active: registry.active,
    }))
}

/// Pre-flight-validate `target_path` before [`activate_workspace_with_current_root`]
/// mutates the registry's `active` pointer, by running the exact
/// initialization `ao-persistence` performs at process startup
/// ([`PersistenceLayer::init_with_root`]) against it, rather than a
/// hand-rolled directory/file checklist that could silently drift out of
/// sync with what actually has to succeed at startup.
///
/// Reusing the real init path is also what makes this safe to run against
/// an empty-but-writable directory: `init_with_root` scaffolds the full
/// directory tree and opens (creating) the SQLite search index as a side
/// effect of succeeding, so a probe that passes leaves the target already
/// initialized for the app's next launch — not just theoretically openable.
/// The returned `PersistenceLayer` (and the SQLite connection it holds) is
/// dropped immediately on success; nothing from the probe is kept around.
///
/// Short-circuits to success, without opening anything, when `target_path`
/// is lexically the same as `current_root` (this process's own data root).
/// That root is demonstrably already working, since this process is
/// running against it right now — probing it for real would open a SECOND
/// `rusqlite::Connection` onto the SAME SQLite file this process's own
/// `PersistenceLayer` already has open. SQLite's file locking is designed
/// to allow that (multiple connections to one file is the normal case,
/// not a hazard), but `rusqlite::Connection::open` sets no busy timeout, so
/// if this process's own connection happens to be mid-write-transaction at
/// that exact instant, the probe's schema-creation statement would fail
/// immediately with "database is locked" (`SQLITE_BUSY`) instead of
/// waiting — a spurious rejection of a root that is plainly fine. The
/// lexical (not canonicalizing) comparison mirrors [`normalize_lexical`]'s
/// own reasoning: a target path that doesn't exist yet can't be
/// canonicalized, so equality is checked on the un-resolved paths as
/// recorded in the registry and as resolved for this process.
async fn probe_target_data_root(target_path: &Path, current_root: &Path) -> Result<(), AppError> {
    if normalize_lexical(target_path) == normalize_lexical(current_root) {
        return Ok(());
    }

    PersistenceLayer::init_with_root(DataRoot::new(target_path))
        .await
        .map(|_layer| ())
        .map_err(|e| {
            AppError(AoError::WorkspaceActivationTargetUnopenable {
                path: target_path.display().to_string(),
                cause: e.to_string(),
            })
        })
}

// ── POST /workspaces/{id}/duplicate ──────────────────────────────────────────

/// POST /workspaces/{id}/duplicate — copy an existing workspace's on-disk
/// tree to a new path and register the copy under a new name. Does not
/// activate the copy.
pub async fn duplicate_workspace(
    AxumPath(id): AxumPath<String>,
    Json(req): Json<DuplicateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceEntry>), AppError> {
    require_data_root_not_pinned()?;
    let name = require_name(&req.name)?;
    require_absolute_path(&req.path)?;

    let mut registry = load_registry()?;
    let source = registry
        .workspaces
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .ok_or_else(|| workspace_not_found(&id))?;

    require_no_path_collision(&registry, &req.path)?;
    require_path_creatable(&req.path).await?;

    if !tokio::fs::try_exists(&source.path).await.unwrap_or(false) {
        return Err(AppError(AoError::ValidationError(format!(
            "source workspace path no longer exists on disk: {}",
            source.path.display()
        ))));
    }

    copy_workspace_tree(source.path.clone(), req.path.clone())
        .await
        .map_err(map_directory_creation_error)?;

    // Duplicate copies the source workspace's visual identity as-is —
    // unlike create, there is no "absent means derive/default" case here,
    // since a duplicate has a well-defined source to copy from.
    let entry = WorkspaceEntry {
        id: Uuid::new_v4().to_string(),
        name,
        path: req.path,
        color: source.color,
        emoji: source.emoji,
    };
    registry.workspaces.push(entry.clone());
    save_registry(&registry)?;

    Ok((StatusCode::CREATED, Json(entry)))
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Most of these tests exercise only the pure validation and filesystem-copy
// logic below the registry layer, not the handlers' registry reads/writes
// directly, since `load_registry`/`save_registry` are pinned to the real
// `$HOME` (see `ao_protocol::workspaces::registry_path`) by default.
//
// The `pinned_data_root_guard` tests below are the exception: they drive a
// real handler end to end, via `ao_protocol::workspaces::registry_path`'s
// `test-util`-feature-gated override (see that function's doc comment),
// which points registry resolution at a temp file instead.

#[cfg(test)]
mod tests {
    use super::*;

    // `AppError` doesn't implement `Debug` (see `crate::error::AppError`), so
    // `Result::unwrap()` can't be used directly against it — this mirrors the
    // `unwrap_ok` helper other route test modules (e.g. `preferences.rs`) use
    // for the same reason.
    fn expect_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

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

    #[test]
    fn require_name_trims_and_accepts_nonempty() {
        assert_eq!(
            expect_ok(require_name("  My Workspace  ")),
            "My Workspace"
        );
    }

    #[test]
    fn require_name_rejects_empty_and_whitespace() {
        assert!(require_name("").is_err());
        assert!(require_name("   ").is_err());
    }

    #[test]
    fn require_absolute_path_rejects_relative() {
        assert!(require_absolute_path(Path::new("relative/dir")).is_err());
        assert!(require_absolute_path(Path::new("./relative")).is_err());
    }

    #[test]
    fn require_absolute_path_accepts_absolute() {
        assert!(require_absolute_path(Path::new("/tmp/some-workspace")).is_ok());
    }

    #[test]
    fn normalize_lexical_collapses_trailing_slash_and_cur_dir() {
        assert_eq!(
            normalize_lexical(Path::new("/tmp/workspace/")),
            normalize_lexical(Path::new("/tmp/workspace"))
        );
        assert_eq!(
            normalize_lexical(Path::new("/tmp/./workspace")),
            normalize_lexical(Path::new("/tmp/workspace"))
        );
    }

    #[test]
    fn path_collision_detected_against_registered_workspace() {
        let registry = registry_with(vec![entry("a", "/tmp/ws-a")]);
        assert!(require_no_path_collision(&registry, Path::new("/tmp/ws-a")).is_err());
        // Trailing slash shouldn't dodge the collision check.
        assert!(require_no_path_collision(&registry, Path::new("/tmp/ws-a/")).is_err());
        assert!(require_no_path_collision(&registry, Path::new("/tmp/ws-b")).is_ok());
    }

    /// The registry-collision message is untouched by the create-or-adopt
    /// change — only the (formerly) "already exists and is not empty"
    /// wording changed. A path colliding with an already-registered
    /// workspace must still produce this exact string.
    #[test]
    fn path_collision_message_unchanged() {
        let registry = registry_with(vec![entry("a", "/tmp/ws-a")]);
        let err = require_no_path_collision(&registry, Path::new("/tmp/ws-a")).unwrap_err();
        match err.0 {
            AoError::ValidationError(msg) => {
                assert_eq!(msg, "a workspace is already registered at /tmp/ws-a")
            }
            other => panic!("expected AoError::ValidationError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn path_creatable_allows_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist-yet");
        assert!(require_path_creatable(&missing).await.is_ok());
    }

    #[tokio::test]
    async fn path_creatable_allows_existing_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(require_path_creatable(tmp.path()).await.is_ok());
    }

    #[tokio::test]
    async fn path_creatable_rejects_existing_nonempty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "hi").unwrap();
        let err = require_path_creatable(tmp.path()).await.unwrap_err();
        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn path_creatable_rejects_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, "hi").unwrap();
        let err = require_path_creatable(&file_path).await.unwrap_err();
        assert!(matches!(err.0, AoError::ValidationError(_)));
    }

    /// Populate `root` with the CORE_DATA_ROOT_DIRS subset that makes it
    /// look like an existing, valid Launchpad data root, plus a marker file
    /// whose contents an adopt must never touch.
    fn seed_valid_data_root(root: &Path) {
        for name in ao_persistence::paths::CORE_DATA_ROOT_DIRS {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("agents").join("existing.yaml"), "id: existing").unwrap();
    }

    #[tokio::test]
    async fn classify_create_target_creatable_for_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist-yet");
        assert!(matches!(
            classify_create_target(&missing).await,
            Ok(CreateTarget::Creatable)
        ));
    }

    #[tokio::test]
    async fn classify_create_target_creatable_for_existing_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            classify_create_target(tmp.path()).await,
            Ok(CreateTarget::Creatable)
        ));
    }

    #[tokio::test]
    async fn classify_create_target_adopts_existing_valid_root_without_mutating_it() {
        let tmp = tempfile::tempdir().unwrap();
        seed_valid_data_root(tmp.path());
        let marker = tmp.path().join("agents").join("existing.yaml");
        let before = std::fs::read_to_string(&marker).unwrap();
        let entries_before: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        assert!(matches!(
            classify_create_target(tmp.path()).await,
            Ok(CreateTarget::ExistingDataRoot)
        ));

        // Read-only: neither the marker file's contents nor the set of
        // top-level entries changed.
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), before);
        let entries_after: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries_before, entries_after);
    }

    #[tokio::test]
    async fn classify_create_target_rejects_nonempty_non_root_dir_with_exact_message() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("random.txt"), "not a workspace").unwrap();

        let err = classify_create_target(tmp.path()).await.unwrap_err();
        match err.0 {
            AoError::ValidationError(msg) => assert_eq!(msg, NOT_ADOPTABLE_MESSAGE),
            other => panic!("expected AoError::ValidationError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn classify_create_target_rejects_existing_file_with_same_message() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, "hi").unwrap();

        let err = classify_create_target(&file_path).await.unwrap_err();
        match err.0 {
            AoError::ValidationError(msg) => assert_eq!(msg, NOT_ADOPTABLE_MESSAGE),
            other => panic!("expected AoError::ValidationError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn copy_workspace_tree_copies_nested_files() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("agents")).unwrap();
        std::fs::write(src.path().join("agents").join("a.yaml"), "id: a").unwrap();
        std::fs::write(src.path().join("top.txt"), "top-level").unwrap();

        let dst = tempfile::tempdir().unwrap();
        let dst_path = dst.path().join("copy");

        copy_workspace_tree(src.path().to_path_buf(), dst_path.clone())
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dst_path.join("agents").join("a.yaml")).unwrap(),
            "id: a"
        );
        assert_eq!(
            std::fs::read_to_string(dst_path.join("top.txt")).unwrap(),
            "top-level"
        );
    }

    // `GET /workspaces/active`'s degraded-payload shape, exercised via
    // `build_active_workspace_response` rather than the real async handler
    // — see that function's doc comment for why the handler itself, whose
    // registry access is pinned to the real `$HOME`, can't be driven with a
    // synthetic corrupt file in a test.
    #[test]
    fn active_workspace_response_carries_degraded_state_and_still_has_a_resolved_path() {
        let degraded = WorkspaceRegistryDegraded {
            reason: "Your workspace list is damaged, so Launchpad Studio opened the default \
                workspace instead. Nothing has been changed or deleted."
                .to_string(),
            detail: "/home/user/.launchpad_studio/workspaces.json exists but is not a valid \
                workspace registry (...)"
                .to_string(),
        };

        // Mirrors what `compute_workspace_root_with_provenance` actually
        // produces once a registry load is degraded: `HomeDefault`
        // provenance (never a false `Registry`), paired with `Some(degraded)`.
        let response = build_active_workspace_response(
            PathBuf::from("/home/user/.launchpad_studio"),
            RootProvenance::HomeDefault,
            Some(degraded.clone()),
            &[],
            None,
        );

        // This is the 200-with-degraded-fields shape required of
        // `GET /workspaces/active`: a resolved path, no error, and the
        // degraded state populated rather than silently dropped.
        assert_eq!(response.path, PathBuf::from("/home/user/.launchpad_studio"));
        assert_eq!(response.degraded, Some(degraded));
        // `HomeDefault` (not `Registry`), so no name lookup is attempted —
        // there is no real registry entry backing this path.
        assert_eq!(response.name, None);
        assert_eq!(response.startup_fallback, None);
    }

    #[test]
    fn active_workspace_response_has_no_degraded_state_for_a_healthy_registry_entry() {
        let workspaces = vec![WorkspaceEntry {
            id: "work".to_string(),
            name: "Work".to_string(),
            path: PathBuf::from("/tmp/work-root"),
            color: derive_default_color("work").to_string(),
            emoji: None,
        }];

        let response = build_active_workspace_response(
            PathBuf::from("/tmp/work-root"),
            RootProvenance::Registry,
            None,
            &workspaces,
            None,
        );

        assert_eq!(response.degraded, None);
        assert_eq!(response.name, Some("Work".to_string()));
        assert_eq!(response.startup_fallback, None);
    }

    // `degraded` (a corrupt registry FILE) and `startup_fallback` (a data
    // root that failed to INITIALIZE) are independent failure modes — see
    // `ActiveWorkspaceResponse::startup_fallback`'s doc comment. This test
    // is the one place that proves they can be populated at the same time
    // without either field clobbering the other.
    #[test]
    fn active_workspace_response_carries_startup_fallback_independently_of_degraded() {
        let fallback = StartupFallback {
            failed_root: PathBuf::from("/registry/root"),
            fallback_root: PathBuf::from("/home/user/.launchpad_studio"),
            error: "permission denied".to_string(),
        };

        let response = build_active_workspace_response(
            PathBuf::from("/home/user/.launchpad_studio"),
            RootProvenance::HomeDefault,
            None,
            &[],
            Some(fallback.clone()),
        );

        assert_eq!(response.startup_fallback, Some(fallback));
        assert_eq!(response.degraded, None);
    }

    // ── `require_data_root_not_pinned` reachability ─────────────────────
    //
    // Every test below holds `crate::routes::env_lock::ENV_LOCK` for its
    // full mutate-then-call window — `LAUNCHPAD_STUDIO_DATA_DIR` is real
    // process-wide state, and the `ao-server` `--lib` test binary runs every
    // `#[cfg(test)]` module in this crate concurrently on multiple threads
    // within one process. See that lock's doc comment in `routes/mod.rs`.

    /// Restore-on-drop guard for `LAUNCHPAD_STUDIO_DATA_DIR`, mirroring
    /// `ao_protocol::data_root`'s own private `EnvGuard` (this crate can't
    /// reuse that one — it's private to `ao-protocol`'s test module).
    struct DataRootEnvGuard {
        prior: Option<String>,
    }

    impl DataRootEnvGuard {
        fn set(value: &str) -> Self {
            let prior = std::env::var("LAUNCHPAD_STUDIO_DATA_DIR").ok();
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", value);
            Self { prior }
        }

        fn unset() -> Self {
            let prior = std::env::var("LAUNCHPAD_STUDIO_DATA_DIR").ok();
            std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
            Self { prior }
        }
    }

    impl Drop for DataRootEnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", v),
                None => std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR"),
            }
        }
    }

    #[test]
    fn require_data_root_not_pinned_allows_unset_env_var() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_guard = DataRootEnvGuard::unset();

        assert!(require_data_root_not_pinned().is_ok());
    }

    #[test]
    fn require_data_root_not_pinned_refuses_pinned_env_var_with_structured_detail() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_guard = DataRootEnvGuard::set("/tmp/pinned-data-root");

        let err = require_data_root_not_pinned().unwrap_err();
        match err.0 {
            AoError::WorkspaceMutationBlockedByPinnedDataRoot { env_var, value } => {
                assert_eq!(env_var, DATA_DIR_ENV_VAR);
                assert_eq!(value, "/tmp/pinned-data-root");
            }
            other => panic!("expected WorkspaceMutationBlockedByPinnedDataRoot, got {other:?}"),
        }
    }

    // ── The startup-fallback exemption ───────────────────────────────────
    //
    // `ao_protocol::data_root`'s fallback record is a `OnceLock` with no
    // reset, so recording one is permanent for the lifetime of this test
    // binary. That's safe here only because `FALLBACK_PIN_ROOT` is used by
    // no other test in this crate: the exemption applies to a pin ONLY when
    // it names that exact root, and every other test pins a different
    // value, so their refusal assertions are unaffected no matter which
    // test runs first. Every caller records the identical value, so the
    // recording is effectively idempotent too.

    /// The data root the fallback-mode tests below pretend startup retreated
    /// to. Deliberately distinct from every other pinned path in this module.
    const FALLBACK_PIN_ROOT: &str = "/tmp/ao-server-startup-fallback-root";

    /// Put this process into the same state a degraded boot leaves it in:
    /// the fallback recorded, and `LAUNCHPAD_STUDIO_DATA_DIR` pinned to the
    /// fallback root. Drives the real production entry point rather than
    /// hand-setting the env var, so these tests can't pass against a pin
    /// that startup would never actually produce.
    fn enter_fallback_mode_for_test() {
        ao_protocol::data_root::pin_startup_fallback_root(
            &ao_protocol::data_root::StartupFallback {
                failed_root: PathBuf::from("/tmp/ao-server-root-that-failed"),
                fallback_root: PathBuf::from(FALLBACK_PIN_ROOT),
                error: "primary root failed to initialize".to_string(),
            },
        );
    }

    /// The guard's whole point after the startup-fallback fix: a pin the
    /// process inflicted on itself must NOT block registry mutations, or a
    /// user who booted into fallback mode has no way back to their real
    /// workspace — the switcher is the only escape route and it runs
    /// entirely through these handlers.
    #[test]
    fn require_data_root_not_pinned_allows_a_self_inflicted_fallback_pin() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_guard = DataRootEnvGuard::unset();

        enter_fallback_mode_for_test();
        assert_eq!(
            std::env::var("LAUNCHPAD_STUDIO_DATA_DIR").ok(),
            Some(FALLBACK_PIN_ROOT.to_string()),
            "precondition: fallback mode pins the env var exactly as startup would"
        );

        assert!(require_data_root_not_pinned().is_ok());
    }

    /// The complement, and the property the dual-worktree contract depends
    /// on: once a fallback has been recorded, a pin naming any OTHER root is
    /// still a deliberate operator pin and must still be refused. Without
    /// this, the exemption above would degrade into "any pin is allowed once
    /// anything ever fell back".
    #[test]
    fn require_data_root_not_pinned_still_refuses_a_user_pin_after_a_fallback_was_recorded() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_guard = DataRootEnvGuard::unset();

        enter_fallback_mode_for_test();
        let _user_pin = DataRootEnvGuard::set("/tmp/ao-server-user-pinned-root");

        let err = require_data_root_not_pinned().unwrap_err();
        match err.0 {
            AoError::WorkspaceMutationBlockedByPinnedDataRoot { env_var, value } => {
                assert_eq!(env_var, DATA_DIR_ENV_VAR);
                assert_eq!(value, "/tmp/ao-server-user-pinned-root");
            }
            other => panic!("expected WorkspaceMutationBlockedByPinnedDataRoot, got {other:?}"),
        }
    }

    /// End-to-end proof that the exemption reaches a real handler: with the
    /// process in fallback mode, `activate_workspace_with_current_root` must
    /// get past the guard and actually persist the new pointer — that
    /// activation IS the escape from fallback mode. Uses the activate path
    /// specifically because its pre-flight probe is what makes escaping safe
    /// (see `probe_target_data_root`), and a seeded registry via the
    /// `test-util` path override, never the real one.
    #[tokio::test]
    async fn activate_handler_escapes_fallback_mode_and_persists_the_pointer() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env_guard = DataRootEnvGuard::unset();

        let tmp = tempfile::tempdir().expect("tempdir");
        let target_root = tmp.path().join("real-workspace");
        PersistenceLayer::init_with_root(DataRoot::new(&target_root))
            .await
            .expect("seed a real, already-initialized data root");

        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![
            entry("fallback", FALLBACK_PIN_ROOT),
            entry("target", target_root.to_str().unwrap()),
        ]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());

        enter_fallback_mode_for_test();

        // The guard runs inside `activate_workspace`, above this function —
        // assert it directly here, since this test drives the inner core
        // (which takes an explicit `current_root`, see the section header
        // above) rather than the axum handler.
        expect_ok(require_data_root_not_pinned());

        let current_root = PathBuf::from(FALLBACK_PIN_ROOT);
        let result =
            activate_workspace_with_current_root("target".to_string(), &current_root).await;

        let Json(response) = expect_ok(result);
        assert_eq!(response.active, "target");
        let on_disk: WorkspaceRegistry =
            serde_json::from_str(&std::fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(
            on_disk.active, "target",
            "escaping fallback mode must actually persist the new active pointer"
        );
    }

    /// End-to-end reachability proof for the pinned-data-root guard: drives
    /// the REAL `rename_workspace` handler — not
    /// `require_data_root_not_pinned` in isolation — with
    /// `LAUNCHPAD_STUDIO_DATA_DIR` pinned, and asserts it
    /// refuses before ever touching the registry file.
    ///
    /// The registry path (via the `test-util` override) is seeded with
    /// GARBAGE content specifically so a guard bug that fell through to
    /// `load_registry()` would be caught here too: garbage content makes
    /// `load_registry()` fail with `AoError::WorkspaceRegistryCorrupt`, a
    /// DIFFERENT variant than the guard's — so this test would fail loudly
    /// (wrong variant) rather than silently passing for the wrong reason if
    /// the guard call were ever removed or reordered past the registry read.
    #[tokio::test]
    async fn rename_workspace_handler_refuses_when_pinned_and_never_touches_registry() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let registry_path = tmp.path().join("workspaces.json");
        let garbage: &[u8] = b"{ not valid json at all }}}";
        std::fs::write(&registry_path, garbage).expect("seed garbage registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let _env_guard = DataRootEnvGuard::set("/tmp/pinned-data-root-e2e");

        let result = rename_workspace(
            AxumPath("does-not-matter".to_string()),
            Json(RenameWorkspaceRequest {
                name: "New Name".to_string(),
                color: None,
                emoji: None,
            }),
        )
        .await;

        match result {
            Ok(_) => panic!("expected the mutation to be refused, but it succeeded"),
            Err(AppError(AoError::WorkspaceMutationBlockedByPinnedDataRoot { env_var, value })) => {
                assert_eq!(env_var, DATA_DIR_ENV_VAR);
                assert_eq!(value, "/tmp/pinned-data-root-e2e");
            }
            Err(AppError(other)) => {
                panic!("expected WorkspaceMutationBlockedByPinnedDataRoot, got {other:?}")
            }
        }

        assert_eq!(
            std::fs::read(&registry_path).unwrap(),
            garbage,
            "a refused mutation must never touch the registry file on disk"
        );
    }

    /// Negative case for the same guard, driving the same real handler:
    /// with `LAUNCHPAD_STUDIO_DATA_DIR` unset, `rename_workspace` must
    /// proceed past `require_data_root_not_pinned` and reach the registry
    /// exactly as it did before this guard existed — proven against a
    /// registry seeded via the `test-util` path override, never the real
    /// `~/.launchpad_studio/workspaces.json`.
    #[tokio::test]
    async fn rename_workspace_handler_proceeds_normally_when_not_pinned() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let registry_path = tmp.path().join("workspaces.json");
        let mut seeded = registry_with(vec![entry("work", "/tmp/work-root")]);
        // Give the seeded entry a real emoji so "leaves it unchanged" is
        // actually exercised, rather than trivially true against `None`.
        seeded.workspaces[0].emoji = Some("🚀".to_string());
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let _env_guard = DataRootEnvGuard::unset();

        let original_color = seeded.workspaces[0].color.clone();
        let original_emoji = seeded.workspaces[0].emoji.clone();

        // `color` absent, `emoji` absent (outer `None` = "leave
        // unchanged") — the rename must leave both fields exactly as they
        // were, not reset them.
        let result = rename_workspace(
            AxumPath("work".to_string()),
            Json(RenameWorkspaceRequest {
                name: "Renamed".to_string(),
                color: None,
                emoji: None,
            }),
        )
        .await;

        let Json(renamed) = expect_ok(result);
        assert_eq!(renamed.name, "Renamed");
        assert_eq!(
            renamed.color, original_color,
            "an absent color in the rename request must leave the existing color unchanged"
        );
        assert_eq!(
            renamed.emoji, original_emoji,
            "an absent emoji in the rename request must leave the existing emoji unchanged"
        );

        let on_disk: WorkspaceRegistry =
            serde_json::from_str(&std::fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(
            on_disk
                .workspaces
                .iter()
                .find(|e| e.id == "work")
                .expect("renamed entry still present")
                .name,
            "Renamed",
            "the handler must have reached save_registry and persisted the rename"
        );
    }

    /// `rename_workspace` with `color`/`emoji` both present in the request
    /// must overwrite the existing values — the complement of the
    /// leave-unchanged case above.
    #[tokio::test]
    async fn rename_workspace_handler_updates_color_and_emoji_when_present() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![entry("work", "/tmp/work-root")]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let _env_guard = DataRootEnvGuard::unset();

        let result = rename_workspace(
            AxumPath("work".to_string()),
            Json(RenameWorkspaceRequest {
                name: "Renamed".to_string(),
                color: Some("#123456".to_string()),
                // Outer `Some` = "field present", inner `Some` = "a value"
                // — sets the emoji.
                emoji: Some(Some("🚀".to_string())),
            }),
        )
        .await;

        let Json(renamed) = expect_ok(result);
        assert_eq!(renamed.color, "#123456");
        assert_eq!(renamed.emoji, Some("🚀".to_string()));
    }

    /// `rename_workspace` with an explicit JSON `null` emoji must clear a
    /// previously-set emoji — the third state `double_option` exists for,
    /// distinct from both "absent" (leave unchanged, tested above) and "a
    /// string" (set, tested above). Deserializes a raw JSON body rather
    /// than constructing `RenameWorkspaceRequest` as a Rust literal, since
    /// only real JSON can distinguish "the client sent literal `null`"
    /// from "the client sent nothing".
    #[tokio::test]
    async fn rename_workspace_handler_clears_emoji_on_explicit_json_null() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let registry_path = tmp.path().join("workspaces.json");
        let mut seeded = registry_with(vec![entry("work", "/tmp/work-root")]);
        seeded.workspaces[0].emoji = Some("🚀".to_string());
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let _env_guard = DataRootEnvGuard::unset();

        let req: RenameWorkspaceRequest =
            serde_json::from_str(r#"{"name":"Renamed","emoji":null}"#)
                .expect("must deserialize an explicit null emoji");
        assert_eq!(
            req.emoji,
            Some(None),
            "an explicit JSON null must deserialize to Some(None) — present-and-clear, not \
             the absent-field None"
        );

        let result = rename_workspace(AxumPath("work".to_string()), Json(req)).await;

        let Json(renamed) = expect_ok(result);
        assert_eq!(
            renamed.emoji, None,
            "an explicit null emoji in the rename request must clear a previously-set emoji"
        );

        let on_disk = std::fs::read_to_string(&registry_path).unwrap();
        assert!(
            !on_disk.contains("emoji"),
            "a cleared emoji must be omitted from the persisted JSON, not written as null — \
             got: {on_disk}"
        );
    }

    /// `create_workspace` with no `color`/`emoji` in the request must derive
    /// a color from the newly-generated id and leave `emoji` unset —
    /// driven through the real handler (not just unit-testing
    /// `derive_default_color` in isolation) so a regression that stops the
    /// handler from calling it is actually caught here.
    #[tokio::test]
    async fn create_workspace_handler_derives_color_and_leaves_emoji_none_when_absent() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed empty registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let _env_guard = DataRootEnvGuard::unset();

        let target = tmp.path().join("new-workspace-root");
        let result = create_workspace(Json(CreateWorkspaceRequest {
            name: "New".to_string(),
            path: target,
            color: None,
            emoji: None,
        }))
        .await;

        let (_, Json(created)) = expect_ok(result);
        assert_eq!(
            created.workspace.emoji, None,
            "an absent emoji in the create request must persist as unset, not a fabricated \
             default"
        );
        assert_eq!(
            created.workspace.color,
            derive_default_color(&created.workspace.id)
        );
    }

    /// `create_workspace` with explicit `color`/`emoji` must use them
    /// verbatim rather than deriving/defaulting.
    #[tokio::test]
    async fn create_workspace_handler_uses_explicit_color_and_emoji_when_present() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed empty registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let _env_guard = DataRootEnvGuard::unset();

        let target = tmp.path().join("new-workspace-root");
        let result = create_workspace(Json(CreateWorkspaceRequest {
            name: "New".to_string(),
            path: target,
            color: Some("#ABCDEF".to_string()),
            emoji: Some("🎯".to_string()),
        }))
        .await;

        let (_, Json(created)) = expect_ok(result);
        assert_eq!(created.workspace.color, "#ABCDEF");
        assert_eq!(created.workspace.emoji, Some("🎯".to_string()));
    }

    /// `duplicate_workspace` must copy the source entry's `color`/`emoji`
    /// verbatim onto the new entry.
    #[tokio::test]
    async fn duplicate_workspace_handler_copies_source_color_and_emoji() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let source_root = tmp.path().join("source-root");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("marker.txt"), "hi").unwrap();

        let mut source = entry("work", source_root.to_str().unwrap());
        source.color = "#654321".to_string();
        source.emoji = Some("🛰️".to_string());
        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![source]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let _env_guard = DataRootEnvGuard::unset();

        let dest = tmp.path().join("dest-root");
        let result = duplicate_workspace(
            AxumPath("work".to_string()),
            Json(DuplicateWorkspaceRequest {
                name: "Copy".to_string(),
                path: dest,
            }),
        )
        .await;

        let (_, Json(duplicated)) = expect_ok(result);
        assert_eq!(duplicated.color, "#654321");
        assert_eq!(duplicated.emoji, Some("🛰️".to_string()));
    }

    // ── `activate_workspace`'s pre-flight probe ──────────────────────────
    //
    // Driven via `activate_workspace_with_current_root` (not the
    // `activate_workspace` axum handler itself) with a synthetic
    // `current_root`, for the same reason `get_active_workspace` can't be
    // exercised end to end — see that function's doc comment and
    // `activate_workspace_with_current_root`'s own doc comment.

    /// A target root that already looks like a previously-initialized real
    /// workspace (populated by running the real init path once as setup,
    /// mirroring what a prior server launch would have left behind) must
    /// still activate successfully and update the registry's pointer.
    #[tokio::test]
    async fn activate_handler_succeeds_against_a_valid_populated_root() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let target_root = tmp.path().join("populated-target");
        PersistenceLayer::init_with_root(DataRoot::new(&target_root))
            .await
            .expect("seed a real, already-initialized data root");

        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![
            entry("work", tmp.path().join("current-root").to_str().unwrap()),
            entry("target", target_root.to_str().unwrap()),
        ]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());

        let current_root = tmp.path().join("current-root");
        let result =
            activate_workspace_with_current_root("target".to_string(), &current_root).await;

        let Json(response) = expect_ok(result);
        assert_eq!(response.active, "target");
        let on_disk: WorkspaceRegistry =
            serde_json::from_str(&std::fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(on_disk.active, "target", "pointer must be persisted");
    }

    /// An empty-but-writable target must activate successfully AND come out
    /// initialized afterward — the probe's deliberate second benefit. This
    /// is the exact "empty directory scaffolding, no search_index.sqlite3"
    /// shape that used to brick the app permanently.
    #[tokio::test]
    async fn activate_handler_succeeds_against_empty_root_and_leaves_it_initialized() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let target_root = tmp.path().join("empty-target");
        std::fs::create_dir_all(&target_root).unwrap();
        let search_index_path = DataRoot::new(&target_root).search_index_path();
        assert!(
            !search_index_path.exists(),
            "precondition: target must start uninitialized"
        );

        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![
            entry("work", tmp.path().join("current-root").to_str().unwrap()),
            entry("target", target_root.to_str().unwrap()),
        ]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());

        let current_root = tmp.path().join("current-root");
        let result =
            activate_workspace_with_current_root("target".to_string(), &current_root).await;

        let Json(_) = expect_ok(result);
        assert!(
            search_index_path.exists(),
            "probing an empty root must leave it initialized for the next launch"
        );
    }

    /// A target that is a FILE, not a directory, can never be turned into a
    /// usable data root — `ensure_directories` will fail trying to create a
    /// subdirectory under it. Activation must refuse with the new typed
    /// error and must not touch the registry file at all.
    #[tokio::test]
    async fn activate_handler_rejects_unusable_root_and_leaves_registry_untouched() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let target_root = tmp.path().join("not-a-directory");
        std::fs::write(&target_root, b"this is a file, not a data root").unwrap();

        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![
            entry("work", tmp.path().join("current-root").to_str().unwrap()),
            entry("target", target_root.to_str().unwrap()),
        ]);
        let seeded_json = serde_json::to_string(&seeded).unwrap();
        std::fs::write(&registry_path, &seeded_json).expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());
        let before = std::fs::read(&registry_path).unwrap();

        let current_root = tmp.path().join("current-root");
        let result =
            activate_workspace_with_current_root("target".to_string(), &current_root).await;

        match result {
            Ok(_) => panic!("expected activation against an unusable root to be refused"),
            Err(AppError(AoError::WorkspaceActivationTargetUnopenable { path, cause })) => {
                assert_eq!(path, target_root.display().to_string());
                assert!(!cause.is_empty());
            }
            Err(AppError(other)) => {
                panic!("expected WorkspaceActivationTargetUnopenable, got {other:?}")
            }
        }

        let after = std::fs::read(&registry_path).unwrap();
        assert_eq!(
            before, after,
            "a refused activation must never touch the registry file on disk"
        );
    }

    /// When the target IS this process's own current root, the probe must
    /// short-circuit rather than actually opening anything — proven here by
    /// pointing both `current_root` and the target at a path that would
    /// fail a real probe (a file, not a directory). If the short-circuit
    /// weren't wired up, this activation would fail exactly like the
    /// unusable-root test above; instead it must succeed.
    #[tokio::test]
    async fn activate_handler_short_circuits_probe_when_target_is_current_root() {
        let _lock = crate::routes::env_lock::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().expect("tempdir");
        let self_path = tmp.path().join("self-root-but-a-file");
        std::fs::write(&self_path, b"not a directory").unwrap();

        let registry_path = tmp.path().join("workspaces.json");
        let seeded = registry_with(vec![entry("self", self_path.to_str().unwrap())]);
        std::fs::write(&registry_path, serde_json::to_string(&seeded).unwrap())
            .expect("seed valid registry file");
        let _registry_override =
            ao_protocol::workspaces::override_registry_path_for_test(registry_path.clone());

        // current_root == the target's own path, lexically.
        let result = activate_workspace_with_current_root("self".to_string(), &self_path).await;

        let Json(response) = expect_ok(result);
        assert_eq!(response.active, "self");
    }
}
