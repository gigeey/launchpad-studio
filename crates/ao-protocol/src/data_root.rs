use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::error::AoError;
use crate::workspaces;

/// Env var name used to override the user data root directory.
///
/// Setting this redirects all per-user state (agents, transcripts, memories,
/// preferences, scheduled tasks, etc.) to a sibling location, which is how
/// the parallel-track worktree workflow keeps two checkouts from clobbering
/// each other's runtime state.
///
/// When set to a non-empty value this always wins, even over an explicit
/// workspace selection recorded in the [`workspaces`] registry — see
/// [`resolve_data_root`] for why.
pub const DATA_DIR_ENV_VAR: &str = "LAUNCHPAD_STUDIO_DATA_DIR";

/// Default sub-directory of the user's home used when no env override is
/// set and no workspace registry entry applies.
pub const DEFAULT_DATA_DIR_NAME: &str = ".launchpad_studio";

/// Which precedence branch of [`resolve_data_root`] produced a resolved data
/// root. Returned alongside the path by [`resolve_data_root_with_provenance`]
/// so callers — in particular the server's workspace-indicator route — can
/// report exactly which branch the resolver took instead of re-deriving it
/// themselves (which is how the indicator UI ended up disagreeing with the
/// resolver: it read the registry's active entry directly, which isn't
/// consulted at all once the env var branch fires).
///
/// Serializes as a lowercase snake_case string (`"env_override"`,
/// `"fallback"`, `"registry"`, `"home_default"`) for use directly in API
/// responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootProvenance {
    /// Resolved from a non-empty `LAUNCHPAD_STUDIO_DATA_DIR` env var that
    /// the process INHERITED — i.e. a deliberate pin by whoever launched
    /// this process. Callers may treat this as "the operator chose this
    /// root on purpose"; see [`user_pinned_data_root_value`].
    EnvOverride,
    /// Resolved from the same env var, but where this process set that var
    /// on ITSELF during crash recovery — startup couldn't initialize the
    /// root it originally resolved and pinned the default root instead, via
    /// [`pin_startup_fallback_root`]. The path is identical in shape to
    /// [`Self::EnvOverride`]; the difference is entirely one of authorship,
    /// and it matters because a self-inflicted pin must never be treated as
    /// an operator instruction to lock the workspace registry down (that
    /// would leave a user who booted into fallback mode with no way back
    /// out — the registry is the only escape route).
    Fallback,
    /// Resolved from the workspace registry's active entry.
    Registry,
    /// Fell back to `$HOME/.launchpad_studio` because neither of the above
    /// applied.
    HomeDefault,
}

/// Process-wide cache of the registry-or-default portion of data root
/// resolution — i.e. everything except the `LAUNCHPAD_STUDIO_DATA_DIR` env
/// var, which is deliberately excluded from this cache (see
/// [`resolve_data_root`] for why) and is instead re-read live on every call.
///
/// Caching this part is sound ONLY because switching the active workspace
/// requires a full process restart today — there is no in-process hot-swap
/// path from one data root to another. `resolve_data_root` /
/// `resolve_data_root_or_cwd` are called lazily, per-invocation, from
/// roughly twenty production call sites across about ten crates, including
/// hot paths; caching here is what turns that into a single registry-file
/// read per process instead of one per call. If in-process workspace
/// switching is ever implemented, this cache AND every one of those call
/// sites must be revisited first, since they all currently assume the
/// non-env-var portion of the returned path never changes for the lifetime
/// of the process.
static WORKSPACE_ROOT_CACHE: OnceLock<(PathBuf, RootProvenance, Option<workspaces::WorkspaceRegistryDegraded>)> =
    OnceLock::new();

/// Resolve the user data root directory.
///
/// Resolution order:
/// 1. `LAUNCHPAD_STUDIO_DATA_DIR` env var, if set to a non-empty value.
///    This outranks an explicit workspace selection because this repo runs
///    two worktree checkouts side by side under a locked port + data-root
///    contract (main track on port 3001 against `~/.launchpad_studio`,
///    tools track on port 3101 against `~/.launchpad_studio-tools`), driven
///    entirely by this env var. If the workspace registry outranked it, the
///    tools worktree could silently clobber the main track's state (or vice
///    versa) any time the registry's `active` entry didn't happen to agree
///    with which track a process was launched as. This branch is checked
///    live on every call, never cached, precisely so that flipping the env
///    var mid-process (as test harnesses that scope it to a single test
///    case do) takes effect immediately rather than being pinned by
///    whichever value happened to win on the first call.
/// 2. The workspace registry's `active` entry, if the registry loads and
///    that entry resolves to a path — see
///    [`workspaces::load_registry_for_resolution`].
/// 3. `$HOME/.launchpad_studio`.
///
/// This is the single source of truth shared by `ao-protocol` (preferences)
/// and `ao-persistence` (the `DataRoot` filesystem layout). All callers that
/// need the user data root should go through this function. Branches 2 and
/// 3 are cached for the lifetime of the process — see
/// [`WORKSPACE_ROOT_CACHE`] — but branch 1 is not.
///
/// Delegates to [`resolve_data_root_with_provenance`], which is the sole
/// place in the crate that implements this precedence order — this function
/// just discards the provenance and degraded-registry halves of that
/// result. Signature and behavior are unchanged by the addition of
/// degraded-registry reporting to the with-provenance variant.
pub fn resolve_data_root() -> Result<PathBuf, AoError> {
    resolve_data_root_with_provenance().map(|(path, _, _)| path)
}

/// Infallible variant: falls back to the current working directory if the
/// home directory cannot be determined. Use for call sites that previously
/// silently degraded to `PathBuf::from(".")` rather than erroring.
///
/// Shares the same process-wide cache as [`resolve_data_root`] for the
/// registry/default portion — whichever of the two is called first performs
/// that resolution — but reads the env var override live, same as
/// [`resolve_data_root`]. Kept as its own env-check/cache-check/compute
/// sequence (rather than a straight call into
/// [`resolve_data_root_with_provenance`]) so that when the home directory
/// can't be determined, the `"."` fallback is itself cached — matching this
/// function's pre-existing behavior of always producing a usable, stable
/// path rather than ever returning an error.
pub fn resolve_data_root_or_cwd() -> PathBuf {
    if let Some(dir) = live_env_override() {
        return PathBuf::from(dir);
    }
    if let Some((cached, _, _)) = WORKSPACE_ROOT_CACHE.get() {
        return cached.clone();
    }
    let resolved = compute_workspace_root_with_provenance()
        .unwrap_or_else(|_| (PathBuf::from("."), RootProvenance::HomeDefault, None));
    WORKSPACE_ROOT_CACHE.get_or_init(|| resolved).0.clone()
}

/// Resolve the user data root directory, along with which precedence branch
/// produced it and — only when the registry branch's file couldn't be used
/// as-is — why resolution fell back to the default root instead of a real
/// registry entry. See [`resolve_data_root`] for the full precedence order
/// and caching rules, which this function implements identically (it IS the
/// implementation `resolve_data_root` delegates to). Exists so callers that
/// need to explain the resolved path to a user — e.g. the workspace
/// indicator's server route — can report the exact branch the resolver took
/// instead of re-deriving it from the registry themselves, which is unsound
/// once the env var branch fires (the registry is never consulted then).
///
/// The third element of the tuple is `Some` only when the on-disk workspace
/// registry exists but couldn't be read or parsed — see
/// [`workspaces::WorkspaceRegistryDegraded`] and
/// [`workspaces::load_registry_for_resolution`]. It is always `None` when
/// the env var branch fires (branch 1 never consults the registry at all)
/// and when the registry file is simply absent (the normal first-run
/// state).
pub fn resolve_data_root_with_provenance(
) -> Result<(PathBuf, RootProvenance, Option<workspaces::WorkspaceRegistryDegraded>), AoError> {
    if let Some(dir) = live_env_override() {
        let provenance = env_branch_provenance(&dir, startup_fallback().as_ref());
        return Ok((PathBuf::from(dir), provenance, None));
    }
    if let Some(cached) = WORKSPACE_ROOT_CACHE.get() {
        return Ok(cached.clone());
    }
    let resolved = compute_workspace_root_with_provenance()?;
    Ok(WORKSPACE_ROOT_CACHE.get_or_init(|| resolved).clone())
}

/// Live (never cached) read of `LAUNCHPAD_STUDIO_DATA_DIR`. Treats unset or
/// whitespace-only as "not set" so an accidentally-exported empty string
/// doesn't resolve to an empty path.
fn live_env_override() -> Option<String> {
    std::env::var(DATA_DIR_ENV_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Public wrapper over [`live_env_override`]: whether
/// `LAUNCHPAD_STUDIO_DATA_DIR` is currently pinning the active data root at
/// all — equivalently, whether [`resolve_data_root_with_provenance`] would
/// take its env-var branch right now, since that branch's condition is
/// exactly this check and always wins unconditionally over the
/// registry/default branches when it applies (see that function's doc
/// comment).
///
/// Says nothing about WHO set the var. A caller deciding whether the
/// operator deliberately pinned this root — rather than this process
/// pinning itself during crash recovery — wants
/// [`user_pinned_data_root_value`] instead; this one answers the strictly
/// weaker "is the env branch live" question and is what the resolver itself
/// keys off.
///
/// Exposed as its own entry point — rather than requiring callers that only
/// need this yes/no answer to call [`resolve_data_root_with_provenance`]
/// and match on `provenance` — so such a caller never touches
/// [`WORKSPACE_ROOT_CACHE`]. That distinction matters for a caller like
/// `ao-server`'s workspace-mutation guard: that cache latches onto the
/// registry/default resolution the FIRST time anything in the process calls
/// a `resolve_data_root*` function and never re-resolves for the rest of
/// the process's life, so a caller that only cares about the env-var branch
/// must not risk being answered from a stale cache entry some unrelated
/// code path happened to populate first.
pub fn data_root_env_override_value() -> Option<String> {
    live_env_override()
}

/// The `LAUNCHPAD_STUDIO_DATA_DIR` value pinning this process's data root,
/// but ONLY when that pin came from the environment this process was
/// launched with — `None` both when the var is unset and when the pin is
/// this process's own startup-fallback pin ([`pin_startup_fallback_root`]).
///
/// This is the question a policy check wants, as opposed to
/// [`data_root_env_override_value`]'s "is the var set at all". A deliberate
/// operator pin means "keep this process on exactly this root, and don't
/// let it rewrite shared state that other, differently-pinned processes
/// depend on"; a self-inflicted fallback pin means the opposite — the
/// process is running somewhere the user never asked for, and needs the
/// workspace registry to remain writable so they can get back out. Treating
/// the two identically is what turns a recoverable degraded boot into an
/// unrecoverable one.
///
/// Shares the [`WORKSPACE_ROOT_CACHE`]-avoidance property of
/// [`data_root_env_override_value`]: neither reads the registry, so neither
/// can be answered from — or populate — that cache.
pub fn user_pinned_data_root_value() -> Option<String> {
    let value = live_env_override()?;
    match env_branch_provenance(&value, startup_fallback().as_ref()) {
        RootProvenance::Fallback => None,
        _ => Some(value),
    }
}

/// Classify a live `LAUNCHPAD_STUDIO_DATA_DIR` value as either an inherited
/// operator pin or this process's own startup-fallback pin, given whatever
/// fallback the process recorded (`None` if it never fell back).
///
/// The pin is self-inflicted exactly when the var's current value names the
/// same directory the recorded fallback says startup retreated to. Matching
/// on the recorded path — rather than a bare "did we fall back at all?"
/// flag — keeps the answer honest if anything else in the process (or a
/// wrapper script re-exporting the var) later points the pin somewhere
/// else: only the root this process pinned to itself is exempt, and
/// anything else falls back to [`RootProvenance::EnvOverride`], which is
/// the conservative direction for every caller that gates on it.
///
/// Pure and free of global state so both branches are unit-testable without
/// touching the real environment or the process-wide fallback record.
fn env_branch_provenance(
    env_value: &str,
    recorded_fallback: Option<&StartupFallback>,
) -> RootProvenance {
    match recorded_fallback {
        Some(fallback) if fallback.fallback_root == std::path::Path::new(env_value) => {
            RootProvenance::Fallback
        }
        _ => RootProvenance::EnvOverride,
    }
}

/// The data root used when startup falls back after the normally-resolved
/// root fails to initialize: `$HOME/{DEFAULT_DATA_DIR_NAME}` — the same
/// path [`compute_workspace_root_with_provenance`]'s own `home_default`
/// branch would pick with no env override and no registry entry in play.
/// Kept as its own entry point (rather than requiring the fallback caller
/// to know that formula itself) so the only place that ever hardcodes
/// `DEFAULT_DATA_DIR_NAME` next to `dirs::home_dir()` is this crate.
pub fn default_data_root() -> Result<PathBuf, AoError> {
    dirs::home_dir()
        .map(|home| home.join(DEFAULT_DATA_DIR_NAME))
        .ok_or_else(|| AoError::Internal("Could not determine home directory".into()))
}

/// Diagnostics recorded when this process's `AppState` couldn't be
/// initialized against the data root [`resolve_data_root_with_provenance`]
/// selected, and startup fell back to [`default_data_root`] instead of
/// crashing. `failed_root`/`fallback_root` name the two roots involved and
/// `error` carries the underlying failure's `Display` text, so a client can
/// show both *what* was tried and *why* it didn't work without parsing
/// process logs. Constructed by [`startup_fallback_diagnostics`] and handed
/// to [`record_startup_fallback`] by the app's own startup sequence — see
/// `frontend/src-tauri/src/lib.rs::start_backend_server`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupFallback {
    pub failed_root: PathBuf,
    pub fallback_root: PathBuf,
    pub error: String,
}

/// Decide the [`StartupFallback`] diagnostics (if any) for a startup
/// attempt against `primary_root`: `primary_error` is `Some` exactly when
/// that attempt failed (its `Display` text), in which case this reports the
/// fallback to `fallback_root`; a root that initialized fine (`None`)
/// always reports no fallback, however this function happens to be called.
/// Pure and synchronous — independent of *how* the attempt was actually
/// made (a real `AppState::new()` in production, a plain value in a test)
/// — so the decision is unit-testable without a real `AppState`. See
/// `start_backend_server` in `frontend/src-tauri/src/lib.rs` for the one
/// production call site, which only ever reaches this on the error path: a
/// successful attempt has nothing to report and never computes
/// `fallback_root` at all.
pub fn startup_fallback_diagnostics(
    primary_root: &std::path::Path,
    primary_error: Option<&str>,
    fallback_root: &std::path::Path,
) -> Option<StartupFallback> {
    let error = primary_error?;
    Some(StartupFallback {
        failed_root: primary_root.to_path_buf(),
        fallback_root: fallback_root.to_path_buf(),
        error: error.to_string(),
    })
}

/// Process-wide record of whether this process's startup fell back to
/// [`default_data_root`] — see [`StartupFallback`]. Set at most once, by
/// [`record_startup_fallback`], from the app's own startup sequence before
/// the HTTP server starts accepting requests. Mirrors
/// [`WORKSPACE_ROOT_CACHE`]'s OnceLock-backed shape rather than threading a
/// new field through every `AppState` constructor, so `GET
/// /workspaces/active` can read it as a plain function call — see
/// [`startup_fallback`].
static STARTUP_FALLBACK: OnceLock<Option<StartupFallback>> = OnceLock::new();

/// Record whether this process's startup fell back to the default data
/// root. Called at most once, early in startup. A second call — there
/// shouldn't be one — is silently ignored rather than panicking: losing
/// this diagnostic is far less harmful than crashing over it.
pub fn record_startup_fallback(fallback: Option<StartupFallback>) {
    let _ = STARTUP_FALLBACK.set(fallback);
}

/// The fallback diagnostics recorded by [`record_startup_fallback`], if
/// any. `None` both when startup never fell back AND when nothing has
/// called `record_startup_fallback` yet (e.g. the standalone `ao-server`
/// binary, which doesn't go through the Tauri app's fallback path at all)
/// — the two cases are indistinguishable here by design, since both mean
/// "there is no fallback to report."
pub fn startup_fallback() -> Option<StartupFallback> {
    STARTUP_FALLBACK.get().cloned().flatten()
}

/// Redirect this process's data root to `fallback.fallback_root` after
/// startup failed to initialize the root it originally resolved, recording
/// the diagnostics for the redirect in the same breath.
///
/// The pin works by setting `LAUNCHPAD_STUDIO_DATA_DIR` on this process
/// rather than by handing an explicit root to one constructor, because the
/// resolved root is re-derived independently at roughly twenty call sites
/// across the workspace; the env var is the only lever that moves all of
/// them at once. See `start_backend_server` in
/// `frontend/src-tauri/src/lib.rs` for the sole production caller.
///
/// ORDERING IS LOAD-BEARING and is the entire reason this is one function
/// instead of two calls at the call site: the record is written BEFORE the
/// var is set, so there is no instant at which an observer can see the pin
/// without also seeing that the pin is self-inflicted. Reversed (or split
/// across a `.await`, as the call site's two operations previously were),
/// every provenance check landing in that window reports
/// [`RootProvenance::EnvOverride`] for a root no user asked for — and
/// anything gating on that, such as `ao-server`'s workspace-mutation guard,
/// would refuse the very mutations that let a user escape fallback mode.
pub fn pin_startup_fallback_root(fallback: &StartupFallback) {
    record_startup_fallback(Some(fallback.clone()));
    std::env::set_var(DATA_DIR_ENV_VAR, &fallback.fallback_root);
}

/// Undo [`pin_startup_fallback_root`]'s env var, but ONLY when the var
/// still holds this process's own fallback pin — a pin inherited from the
/// environment is left strictly alone. Returns whether the var was actually
/// removed.
///
/// Needed because the pin is ordinary process environment, and a process
/// relaunching itself passes its environment to the new image unchanged
/// (Tauri's restart spawns the current binary via `std::process::Command`
/// with no `env_clear`, so the child inherits `environ` as mutated by
/// `setenv`). Without this, a fallback-mode pin would outlive the very
/// restart meant to clear it: the relaunched process would resolve the
/// fallback root again — this time with no recorded fallback to explain it,
/// so it would look like a deliberate operator pin — and would silently
/// ignore whichever workspace the user just activated, since the env branch
/// outranks the registry. Call this immediately before triggering a
/// restart; the relaunched process then re-resolves from the registry, as
/// an ordinary launch does.
pub fn unpin_startup_fallback_root() -> bool {
    let Some(value) = live_env_override() else {
        return false;
    };
    if env_branch_provenance(&value, startup_fallback().as_ref()) != RootProvenance::Fallback {
        return false;
    }
    std::env::remove_var(DATA_DIR_ENV_VAR);
    true
}

/// Resolve the registry-or-default portion of the precedence order —
/// branches 2 and 3 of [`resolve_data_root`]'s doc comment — along with
/// which of the two produced the result, and the degraded-registry state
/// (if any) from loading it. Callers have already ruled out the env var
/// branch before reaching here. Backs [`WORKSPACE_ROOT_CACHE`], so the
/// registry file is read at most once per process.
fn compute_workspace_root_with_provenance(
) -> Result<(PathBuf, RootProvenance, Option<workspaces::WorkspaceRegistryDegraded>), AoError> {
    let (registry, degraded) = workspaces::load_registry_for_resolution();

    // When the registry couldn't be used as-is, `registry` is a synthetic
    // single-entry fallback whose `active_path()` would otherwise resolve
    // just like a real registry entry, reporting `RootProvenance::Registry`
    // as if one had actually decided the path. Treat it as if no registry
    // entry applied instead, so `resolve_precedence` falls through to the
    // `home_default` branch below — which is where the synthetic fallback's
    // path always points anyway (both are `$HOME/.launchpad_studio`) — and
    // degraded resolution is reported as `HomeDefault` annotated with
    // `degraded`, never as a false `Registry`. This also keeps
    // `GET /workspaces/active`'s registry-name lookup (gated on
    // `provenance == RootProvenance::Registry`) from firing on a corrupt
    // file it would only fail to re-read.
    let registry_active_path = if degraded.is_some() {
        None
    } else {
        registry.active_path()
    };
    let home_default = dirs::home_dir().map(|home| home.join(DEFAULT_DATA_DIR_NAME));

    // `None` for the fallback record: this function is only ever reached
    // with the env var branch already ruled out, so there is no pin whose
    // authorship could need classifying.
    let (path, provenance) = resolve_precedence(None, None, registry_active_path, home_default)
        .ok_or_else(|| AoError::Internal("Could not determine home directory".into()))?;
    Ok((path, provenance, degraded))
}

/// Pure precedence resolution: env var, then registry active path, then
/// home-directory default. This is the ONLY place in the crate that
/// implements the env → registry → default decision — every public entry
/// point (`resolve_data_root`, `resolve_data_root_or_cwd`,
/// `resolve_data_root_with_provenance`) funnels through it rather than
/// re-deriving the order itself. Isolated from
/// [`compute_workspace_root_with_provenance`] so every branch can be
/// exercised directly in tests without touching the real environment, the
/// registry file, or the process-wide cache. Takes `env_value` and
/// `recorded_fallback` as explicit parameters (rather than reading
/// `std::env::var` and the process-wide fallback record itself) purely so
/// tests can cover the env-var branch — including the operator-pin vs
/// self-inflicted-pin split described on [`env_branch_provenance`] —
/// without mutating global process state.
///
/// Returns `None` only when none of the three sources yield a path — i.e.
/// no (non-empty) env override, no resolvable registry entry, and no home
/// directory to fall back to.
fn resolve_precedence(
    env_value: Option<&str>,
    recorded_fallback: Option<&StartupFallback>,
    registry_active_path: Option<PathBuf>,
    home_default: Option<PathBuf>,
) -> Option<(PathBuf, RootProvenance)> {
    if let Some(v) = env_value {
        if !v.trim().is_empty() {
            return Some((PathBuf::from(v), env_branch_provenance(v, recorded_fallback)));
        }
    }
    if let Some(path) = registry_active_path {
        return Some((path, RootProvenance::Registry));
    }
    home_default.map(|path| (path, RootProvenance::HomeDefault))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::{derive_default_color, WorkspaceEntry, WorkspaceRegistry};

    fn registry_with_active(id: &str, path: &str) -> WorkspaceRegistry {
        WorkspaceRegistry {
            version: 1,
            active: id.to_string(),
            workspaces: vec![WorkspaceEntry {
                id: id.to_string(),
                name: "Test".to_string(),
                path: PathBuf::from(path),
                color: derive_default_color(id).to_string(),
                emoji: None,
            }],
        }
    }

    #[test]
    fn env_var_wins_over_registry_and_default() {
        // This is the exact bug being fixed: the active registry entry
        // ("work" / "/registry/root") disagrees with the env var, and the
        // env var must win on BOTH the path and the reported provenance —
        // a caller must never be able to observe `RootProvenance::Registry`
        // while the env var is set.
        let registry = registry_with_active("work", "/registry/root");
        let resolved = resolve_precedence(
            Some("/env/root"),
            None,
            registry.active_path(),
            Some(PathBuf::from("/home/.launchpad_studio")),
        );
        assert_eq!(
            resolved,
            Some((PathBuf::from("/env/root"), RootProvenance::EnvOverride))
        );
    }

    #[test]
    fn empty_env_var_is_treated_as_unset_and_falls_through_to_registry() {
        let registry = registry_with_active("work", "/registry/root");
        let resolved = resolve_precedence(
            Some(""),
            None,
            registry.active_path(),
            Some(PathBuf::from("/home/.launchpad_studio")),
        );
        assert_eq!(
            resolved,
            Some((PathBuf::from("/registry/root"), RootProvenance::Registry))
        );
    }

    #[test]
    fn whitespace_only_env_var_is_treated_as_unset() {
        let resolved = resolve_precedence(
            Some("   "),
            None,
            None,
            Some(PathBuf::from("/home/.launchpad_studio")),
        );
        assert_eq!(
            resolved,
            Some((
                PathBuf::from("/home/.launchpad_studio"),
                RootProvenance::HomeDefault
            ))
        );
    }

    #[test]
    fn registry_wins_over_default_when_no_env_var() {
        let registry = registry_with_active("work", "/registry/root");
        let resolved = resolve_precedence(
            None,
            None,
            registry.active_path(),
            Some(PathBuf::from("/home/.launchpad_studio")),
        );
        assert_eq!(
            resolved,
            Some((PathBuf::from("/registry/root"), RootProvenance::Registry))
        );
    }

    #[test]
    fn default_wins_when_no_env_var_and_registry_active_unresolved() {
        // active id doesn't match any entry -> active_path() is None
        let registry = WorkspaceRegistry {
            version: 1,
            active: "missing".to_string(),
            workspaces: vec![WorkspaceEntry {
                id: "other".to_string(),
                name: "Other".to_string(),
                path: PathBuf::from("/registry/root"),
                color: derive_default_color("other").to_string(),
                emoji: None,
            }],
        };
        let resolved = resolve_precedence(
            None,
            None,
            registry.active_path(),
            Some(PathBuf::from("/home/.launchpad_studio")),
        );
        assert_eq!(
            resolved,
            Some((
                PathBuf::from("/home/.launchpad_studio"),
                RootProvenance::HomeDefault
            ))
        );
    }

    #[test]
    fn default_wins_when_nothing_else_present() {
        let resolved =
            resolve_precedence(None, None, None, Some(PathBuf::from("/home/.launchpad_studio")));
        assert_eq!(
            resolved,
            Some((
                PathBuf::from("/home/.launchpad_studio"),
                RootProvenance::HomeDefault
            ))
        );
    }

    #[test]
    fn none_when_no_source_resolves() {
        let resolved = resolve_precedence(None, None, None, None);
        assert_eq!(resolved, None);
    }

    // `LAUNCHPAD_STUDIO_DATA_DIR` is real process-wide state. Any test below
    // that touches it (directly, or indirectly by calling `resolve_data_root`
    // / `resolve_data_root_or_cwd`, which read it live) must hold this lock
    // for its duration, or a parallel test-thread flipping the var mid-check
    // would make both tests flaky.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        prior: Option<String>,
    }

    impl EnvGuard {
        fn set(val: &str) -> Self {
            let prior = std::env::var(DATA_DIR_ENV_VAR).ok();
            std::env::set_var(DATA_DIR_ENV_VAR, val);
            Self { prior }
        }

        fn unset() -> Self {
            let prior = std::env::var(DATA_DIR_ENV_VAR).ok();
            std::env::remove_var(DATA_DIR_ENV_VAR);
            Self { prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(DATA_DIR_ENV_VAR, v),
                None => std::env::remove_var(DATA_DIR_ENV_VAR),
            }
        }
    }

    #[test]
    fn env_override_is_read_live_every_call_not_cached() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let _guard = EnvGuard::set("/tmp/workspace-root-a");
        assert_eq!(
            resolve_data_root().unwrap(),
            PathBuf::from("/tmp/workspace-root-a")
        );
        assert_eq!(
            resolve_data_root_or_cwd(),
            PathBuf::from("/tmp/workspace-root-a")
        );

        // Flip the env var without a process restart. If `resolve_data_root`
        // ever started caching this branch in `WORKSPACE_ROOT_CACHE`, this
        // assertion would see the stale "-a" value instead.
        std::env::set_var(DATA_DIR_ENV_VAR, "/tmp/workspace-root-b");
        assert_eq!(
            resolve_data_root().unwrap(),
            PathBuf::from("/tmp/workspace-root-b")
        );
        assert_eq!(
            resolve_data_root_or_cwd(),
            PathBuf::from("/tmp/workspace-root-b")
        );
    }

    #[test]
    fn resolve_data_root_is_cached_and_stable_across_calls() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::unset();

        // With no env override, both calls (and both entry points) must
        // agree once WORKSPACE_ROOT_CACHE has latched onto the
        // registry/default resolution.
        let first = resolve_data_root();
        let second = resolve_data_root();
        assert_eq!(first.ok(), second.ok());

        let via_or_cwd_first = resolve_data_root_or_cwd();
        let via_or_cwd_second = resolve_data_root_or_cwd();
        assert_eq!(via_or_cwd_first, via_or_cwd_second);
    }

    #[test]
    fn provenance_is_env_override_even_when_registry_active_workspace_differs() {
        // This is the exact bug being fixed: `WorkspaceIndicator` used to
        // read the registry's active entry directly, which can name a
        // completely different workspace than the one this process is
        // actually running on once `LAUNCHPAD_STUDIO_DATA_DIR` is set — as
        // it always is under the locked dual-worktree contract. Whatever
        // the real on-disk registry's active workspace happens to be (this
        // test can't inject a fake one — `workspaces::load_registry` is
        // pinned to the real `$HOME`, same constraint documented on
        // `routes::workspaces`'s own test module), the env var must win on
        // both the path AND the reported provenance.
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set("/tmp/workspace-root-provenance-check");

        let (path, provenance, degraded) = resolve_data_root_with_provenance().unwrap();
        assert_eq!(path, PathBuf::from("/tmp/workspace-root-provenance-check"));
        assert_eq!(provenance, RootProvenance::EnvOverride);
        assert_eq!(
            degraded, None,
            "the env override branch never consults the registry, so it can never be degraded"
        );

        // `resolve_data_root` must report the identical path — it's a thin
        // wrapper over `resolve_data_root_with_provenance`, not a second
        // implementation of the precedence order.
        assert_eq!(
            resolve_data_root().unwrap(),
            PathBuf::from("/tmp/workspace-root-provenance-check")
        );
    }

    #[test]
    fn degraded_is_none_when_env_var_is_set_even_if_the_real_registry_is_unreadable() {
        // Required coverage: with `LAUNCHPAD_STUDIO_DATA_DIR` set, branch 1
        // must win before the registry is ever consulted — so regardless of
        // what state the real on-disk registry happens to be in on this
        // machine, `degraded` must always come back `None` here. (This is
        // the same real-`$HOME`-pinning constraint documented on the
        // `provenance_is_env_override_...` test above: we can't inject a
        // fake corrupt registry file into this branch to prove the negative
        // more directly, only observe that this branch never reports one.)
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set("/tmp/workspace-root-degraded-env-check");

        let (_, _, degraded) = resolve_data_root_with_provenance().unwrap();
        assert_eq!(degraded, None);
    }

    #[test]
    fn provenance_branches_are_exhaustively_covered_via_pure_precedence_fn() {
        // `resolve_precedence` is the crate's sole precedence
        // implementation (see its doc comment); the registry and
        // home-default branches aren't independently exercisable through
        // the public, cached API in a unit test (the registry file is
        // pinned to the real `$HOME`), so they're covered here instead —
        // this test just asserts all three `RootProvenance` variants are
        // reachable in principle, one per branch.
        let registry = registry_with_active("work", "/registry/root");

        assert_eq!(
            resolve_precedence(
                Some("/env/root"),
                None,
                registry.active_path(),
                Some(PathBuf::from("/home/.launchpad_studio"))
            )
            .map(|(_, p)| p),
            Some(RootProvenance::EnvOverride)
        );
        assert_eq!(
            resolve_precedence(
                None,
                None,
                registry.active_path(),
                Some(PathBuf::from("/home/.launchpad_studio"))
            )
            .map(|(_, p)| p),
            Some(RootProvenance::Registry)
        );
        assert_eq!(
            resolve_precedence(None, None, None, Some(PathBuf::from("/home/.launchpad_studio")))
                .map(|(_, p)| p),
            Some(RootProvenance::HomeDefault)
        );
    }

    #[test]
    fn data_root_env_override_value_mirrors_the_env_override_provenance_branch() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        {
            let _guard = EnvGuard::set("/tmp/data-root-env-override-value-check");
            assert_eq!(
                data_root_env_override_value(),
                Some("/tmp/data-root-env-override-value-check".to_string())
            );
        }
        {
            let _guard = EnvGuard::unset();
            assert_eq!(data_root_env_override_value(), None);
        }
        {
            let _guard = EnvGuard::set("   ");
            assert_eq!(
                data_root_env_override_value(),
                None,
                "whitespace-only must be treated as unset, matching resolve_precedence"
            );
        }
    }

    // ── `default_data_root` / `startup_fallback_diagnostics` ───────────────
    //
    // Covers the startup-fallback machinery `frontend/src-tauri/src/lib.rs`
    // drives when `AppState::new()` fails against the normally-resolved
    // root. None of this touches a real `~/.launchpad_studio` or
    // `~/.launchpad_studio-tools` directory: `default_data_root_test` points
    // `$HOME` itself at a tempdir first, and `startup_fallback_diagnostics`
    // is pure (it only ever formats the paths it's given, never reads them).

    /// Restore-on-drop guard for `HOME`, mirroring [`EnvGuard`] but for a
    /// different env var — kept separate rather than generalizing
    /// `EnvGuard` so the existing `DATA_DIR_ENV_VAR` call sites above stay
    /// untouched.
    struct HomeEnvGuard {
        prior: Option<String>,
    }

    impl HomeEnvGuard {
        fn set(val: &std::path::Path) -> Self {
            let prior = std::env::var("HOME").ok();
            std::env::set_var("HOME", val);
            Self { prior }
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn default_data_root_joins_home_with_the_default_dir_name() {
        // `default_data_root` reads `dirs::home_dir()`, which resolves
        // `$HOME` on this platform — pointing it at a tempdir (rather than
        // asserting against whatever the real `$HOME` happens to be) is
        // what lets this test run safely under any developer's real home
        // directory without depending on — or touching — it.
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let temp_home = tempfile::tempdir().unwrap();
        let _guard = HomeEnvGuard::set(temp_home.path());

        assert_eq!(
            default_data_root().unwrap(),
            temp_home.path().join(DEFAULT_DATA_DIR_NAME)
        );
    }

    #[test]
    fn startup_fallback_diagnostics_is_none_for_a_root_that_initialized_fine() {
        let temp_primary = tempfile::tempdir().unwrap();
        let temp_fallback = tempfile::tempdir().unwrap();

        assert_eq!(
            startup_fallback_diagnostics(temp_primary.path(), None, temp_fallback.path()),
            None,
            "a successful primary attempt must never report a fallback"
        );
    }

    #[test]
    fn startup_fallback_diagnostics_reports_both_roots_and_the_error_for_a_failed_root() {
        let temp_primary = tempfile::tempdir().unwrap();
        let temp_fallback = tempfile::tempdir().unwrap();

        let diagnostics = startup_fallback_diagnostics(
            temp_primary.path(),
            Some("permission denied"),
            temp_fallback.path(),
        );

        assert_eq!(
            diagnostics,
            Some(StartupFallback {
                failed_root: temp_primary.path().to_path_buf(),
                fallback_root: temp_fallback.path().to_path_buf(),
                error: "permission denied".to_string(),
            })
        );
    }

    // ── Operator pin vs self-inflicted fallback pin ───────────────────────
    //
    // Both look identical on the wire — a non-empty
    // `LAUNCHPAD_STUDIO_DATA_DIR` — so everything below is about proving the
    // two are told apart by AUTHORSHIP, using the recorded startup fallback
    // as the only evidence available. The first two tests are pure; the
    // process-wide ones follow, and are deliberately pinned to a path no
    // other test in this binary uses (see `FALLBACK_PIN_ROOT`).

    fn fallback_to(root: &str) -> StartupFallback {
        StartupFallback {
            failed_root: PathBuf::from("/some/root/that/failed"),
            fallback_root: PathBuf::from(root),
            error: "primary root failed to initialize".to_string(),
        }
    }

    #[test]
    fn env_branch_provenance_is_env_override_when_nothing_fell_back() {
        assert_eq!(
            env_branch_provenance("/env/root", None),
            RootProvenance::EnvOverride
        );
    }

    #[test]
    fn env_branch_provenance_is_fallback_only_for_the_recorded_fallback_root() {
        let recorded = fallback_to("/home/.launchpad_studio");

        assert_eq!(
            env_branch_provenance("/home/.launchpad_studio", Some(&recorded)),
            RootProvenance::Fallback,
            "a pin naming the root startup retreated to is this process's own doing"
        );
        // A pin naming any OTHER root can't have come from this process's
        // fallback — whatever set it, it wasn't `pin_startup_fallback_root`
        // — so it must stay classified as an operator pin. This is the
        // conservative direction: mis-classifying an operator pin as
        // self-inflicted would unlock registry mutations the dual-worktree
        // contract depends on being refused.
        assert_eq!(
            env_branch_provenance("/some/other/root", Some(&recorded)),
            RootProvenance::EnvOverride
        );
    }

    #[test]
    fn resolve_precedence_env_branch_carries_the_authorship_split_through() {
        // The env branch must win on the path either way — the split is
        // purely about which provenance it reports alongside it.
        let recorded = fallback_to("/home/.launchpad_studio");
        let registry = registry_with_active("work", "/registry/root");

        assert_eq!(
            resolve_precedence(
                Some("/home/.launchpad_studio"),
                Some(&recorded),
                registry.active_path(),
                Some(PathBuf::from("/home/.launchpad_studio"))
            ),
            Some((
                PathBuf::from("/home/.launchpad_studio"),
                RootProvenance::Fallback
            ))
        );
        assert_eq!(
            resolve_precedence(
                Some("/env/root"),
                Some(&recorded),
                registry.active_path(),
                Some(PathBuf::from("/home/.launchpad_studio"))
            ),
            Some((PathBuf::from("/env/root"), RootProvenance::EnvOverride))
        );
    }

    /// The root the process-wide tests below pin to. `STARTUP_FALLBACK` is a
    /// `OnceLock` with no reset, so recording a fallback is permanent for
    /// the lifetime of this test binary — which is safe only because this
    /// path is used by no other test here: every other test pins a
    /// different value, and `env_branch_provenance` exempts a pin only when
    /// it names this exact root.
    const FALLBACK_PIN_ROOT: &str = "/tmp/ao-protocol-startup-fallback-root";

    /// Drive the real production pin (`pin_startup_fallback_root`), not
    /// `record_startup_fallback` directly, so these tests exercise the same
    /// record-then-set-var sequence startup uses. Idempotent across tests:
    /// the `OnceLock` ignores the second recording, and every caller here
    /// records the identical value, so the outcome doesn't depend on which
    /// test runs first.
    fn pin_fallback_for_test() -> StartupFallback {
        let fallback = fallback_to(FALLBACK_PIN_ROOT);
        pin_startup_fallback_root(&fallback);
        fallback
    }

    #[test]
    fn self_inflicted_pin_resolves_as_fallback_and_is_not_a_user_pin() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::unset();

        // `pin_startup_fallback_root` sets the env var itself — that IS the
        // pin — so there's no separate `EnvGuard::set` here; the unset guard
        // above is what restores the environment afterward.
        let fallback = pin_fallback_for_test();

        assert_eq!(
            data_root_env_override_value(),
            Some(FALLBACK_PIN_ROOT.to_string()),
            "the pin must be visible to the resolver's env branch"
        );

        let (path, provenance, degraded) = resolve_data_root_with_provenance().unwrap();
        assert_eq!(path, PathBuf::from(FALLBACK_PIN_ROOT));
        assert_eq!(
            provenance,
            RootProvenance::Fallback,
            "a pin this process set on itself must never be reported as an operator pin"
        );
        assert_eq!(degraded, None);

        assert_eq!(
            user_pinned_data_root_value(),
            None,
            "fallback mode must leave registry-mutating callers unblocked"
        );
        assert_eq!(startup_fallback(), Some(fallback));
    }

    #[test]
    fn an_operator_pin_stays_an_operator_pin_even_after_a_fallback_was_recorded() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::unset();

        // Record the fallback first, then pin somewhere else entirely — the
        // shape a genuinely env-pinned process would have if it had ALSO
        // fallen back at some point. The current pin isn't the fallback
        // root, so it's not ours to exempt.
        pin_fallback_for_test();
        let _pin = EnvGuard::set("/tmp/ao-protocol-operator-pin-root");

        let (path, provenance, _) = resolve_data_root_with_provenance().unwrap();
        assert_eq!(path, PathBuf::from("/tmp/ao-protocol-operator-pin-root"));
        assert_eq!(provenance, RootProvenance::EnvOverride);
        assert_eq!(
            user_pinned_data_root_value(),
            Some("/tmp/ao-protocol-operator-pin-root".to_string())
        );
    }

    #[test]
    fn unpin_clears_only_a_self_inflicted_pin() {
        let _lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::unset();

        // Nothing pinned at all: nothing to clear.
        assert!(!unpin_startup_fallback_root());

        pin_fallback_for_test();

        {
            // An operator pin must survive an unpin attempt untouched —
            // otherwise a restart would silently drop the dual-worktree
            // contract's pin and relaunch against the wrong data root.
            let _operator_pin = EnvGuard::set("/tmp/ao-protocol-operator-pin-root");
            assert!(!unpin_startup_fallback_root());
            assert_eq!(
                data_root_env_override_value(),
                Some("/tmp/ao-protocol-operator-pin-root".to_string())
            );
        }

        // Back to the self-inflicted pin: this one must be cleared, so the
        // relaunched process resolves from the registry instead of
        // inheriting fallback mode forever.
        std::env::set_var(DATA_DIR_ENV_VAR, FALLBACK_PIN_ROOT);
        assert!(unpin_startup_fallback_root());
        assert_eq!(data_root_env_override_value(), None);
        assert!(
            !unpin_startup_fallback_root(),
            "unpinning twice must be a no-op, not a double-clear"
        );
    }
}
