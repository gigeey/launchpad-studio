//! Consolidated secret storage for every credential this crate persists:
//! MCP OAuth tokens, Telegram bot tokens, channel-binding secrets, and
//! provider API keys.
//!
//! All four categories share one OS keychain item (macOS Keychain, Windows
//! Credential Manager, Linux Secret Service) — or one JSON file when no
//! keychain is reachable — so unlocking secret access prompts the user at
//! most once per process instead of once per category. Callers use the
//! typed per-category accessors on [`SecretVault`]; the four categories are
//! kept apart on disk by a single tagged blob ([`VaultBlob`]) rather than one
//! flat map, since their value shapes differ (a structured OAuth record for
//! MCP, plain strings for the rest).
//!
//! # Backend selection
//!
//! [`SecretVault::open`] tries the OS keychain first and falls back to a
//! `0o600` JSON file under the data root when no keychain is reachable. Set
//! `LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK` to force the file backend
//! regardless of keychain availability (useful in CI where no keychain
//! daemon is running). The three fallback variables the individual stores
//! this vault consolidates used to check on their own are also honored as
//! aliases, so configuration written before this vault existed keeps
//! working unchanged: `LAUNCHPAD_MCP_STORE_FILE_FALLBACK`,
//! `LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK`,
//! `LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK`.
//!
//! Every process logs which backend it selected exactly once, at `INFO`,
//! the first time [`SecretVault::open`] runs.
//!
//! # No unattended process may ever block on a keychain prompt
//!
//! macOS keychain access-control prompts ("`<app>` wants to use your
//! confidential information...") are granted per requesting-binary code
//! signature. A `cargo build` dev binary is ad-hoc-signed with a signature
//! that changes on every rebuild, so a granted "Always Allow" never
//! survives past the next build; a `cargo test`/`cargo bench` harness
//! binary is worse still, since it never even has a stable name — cargo
//! places it at `target/<profile>/deps/<crate>-<hash>` with a fresh hash
//! per build, so it could never accumulate a durable grant even in
//! principle. Any process in that position that reads an existing vault
//! item is at the mercy of a GUI modal nobody is present to answer, which
//! hangs the process forever.
//!
//! Three layers guard against this, from cheapest to most structural:
//!
//! 1. [`SecretVault::open`] never touches the keychain when a lower-cost
//!    source already answers the question — an explicit force-file
//!    configuration short-circuits before the keychain is even probed for
//!    reachability (see [`file_fallback_forced`] below).
//! 2. `LAUNCHPAD_STUDIO_NO_KEYCHAIN` (or `CI`) is an explicit, unconditional
//!    kill switch: set either and this process falls back to the file
//!    backend without ever probing the keychain, no matter what else is
//!    configured.
//! 3. A cargo test/bench harness binary is detected automatically at
//!    runtime by inspecting `std::env::current_exe()` for the
//!    `deps/<name>-<hash>` shape cargo always uses for them — this is the
//!    one signal that generalizes across crate boundaries. `cfg!(test)`
//!    does NOT substitute for this: it reflects whether *this* crate was
//!    compiled with `--cfg test`, which is false here even when the
//!    running process is a *different* crate's test binary.
//!
//! On top of all three, [`disable_interactive_keychain_prompts`] is a
//! structural (not policy) belt-and-braces guard: it asks macOS itself to
//! fail keychain calls immediately with `errSecInteractionNotAllowed`
//! instead of drawing a prompt, process-wide, for the rest of the current
//! process's life. The tools CLI calls it unconditionally at startup, since
//! it's headless by nature and predominantly invoked by an unattended
//! agent. The server calls it only when
//! [`should_suppress_keychain_prompts`] says to — that is, only when the
//! keychain is already off-limits to this process anyway. A server run with
//! a real user in front of it, debug build or release, is left free to draw
//! a legitimate prompt and have it answered. The Tauri desktop
//! app must never call either helper, since prompting its user for
//! one-time keychain authorization is the legitimate, intended flow there.
//! When suppression turns what would have been a real prompt into a hard
//! failure instead, the conversion from `keyring::Error` to [`VaultError`]
//! logs a `tracing::warn!` naming the cause and both ways out
//! (`LAUNCHPAD_STUDIO_ALLOW_KEYCHAIN_PROMPT=1`, or the file-backed vault via
//! `LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK`) instead of surfacing as an
//! unexplained keychain error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use ao_protocol::data_root::resolve_data_root;
use ao_protocol::error::AoError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::channel_secret_store::{
    CONSOLIDATED_ACCOUNT as LEGACY_CHANNEL_CONSOLIDATED_ACCOUNT, FILE_STORE_NAME as LEGACY_CHANNEL_FILE_NAME,
    KEYRING_SERVICE as LEGACY_CHANNEL_KEYRING_SERVICE,
};
use crate::mcp_token_store::{
    CONSOLIDATED_ACCOUNT as LEGACY_MCP_CONSOLIDATED_ACCOUNT, FILE_STORE_NAME as LEGACY_MCP_FILE_NAME,
    KEYRING_SERVICE as LEGACY_MCP_KEYRING_SERVICE, McpTokenRecord,
};
use crate::telegram_token_store::{
    CONSOLIDATED_ACCOUNT as LEGACY_TELEGRAM_CONSOLIDATED_ACCOUNT, FILE_STORE_NAME as LEGACY_TELEGRAM_FILE_NAME,
    KEYRING_SERVICE as LEGACY_TELEGRAM_KEYRING_SERVICE,
};

const KEYRING_SERVICE: &str = "launchpad_studio_vault";
const FILE_STORE_NAME: &str = "secret_vault.json";

/// Account name for the single keychain item backing every category this
/// vault stores. Consolidating into one item (rather than one per category,
/// or one per secret) means a launch prompts for keychain access at most
/// once per process.
const CONSOLIDATED_ACCOUNT: &str = "__launchpad_secret_vault_v1__";

/// Forces the file backend regardless of keychain availability.
const FILE_FALLBACK_ENV_VAR: &str = "LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK";

/// Force-file variables read by the per-category stores this vault
/// consolidates, honored here as aliases for [`FILE_FALLBACK_ENV_VAR`] so
/// deployments (CI included) that already set one of these keep forcing the
/// file backend without needing to change configuration.
const LEGACY_FILE_FALLBACK_ENV_VARS: &[&str] = &[
    "LAUNCHPAD_MCP_STORE_FILE_FALLBACK",
    "LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK",
    "LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK",
];

/// Explicit kill switch: set this to categorically forbid the current
/// process from ever touching the OS keychain, e.g. a background/unattended
/// agent run where nobody is present to answer a permission prompt.
const NO_KEYCHAIN_ENV_VAR: &str = "LAUNCHPAD_STUDIO_NO_KEYCHAIN";

/// Escape hatch for [`should_suppress_keychain_prompts`]: set this to force
/// interactive keychain prompts back on for the server even when
/// [`keychain_forbidden`] would otherwise suppress them. Exists so a
/// developer debugging a real keychain-backed server run can get the prompt
/// back without a code change.
const ALLOW_KEYCHAIN_PROMPT_ENV_VAR: &str = "LAUNCHPAD_STUDIO_ALLOW_KEYCHAIN_PROMPT";

/// Serializes all keychain blob reads and read-modify-writes, and doubles as
/// the in-memory cache of the decrypted blob so the keychain item is read at
/// most once per process launch. `None` means the blob hasn't been read yet;
/// once populated it is always `Some`, even when the underlying keychain item
/// doesn't exist yet — an empty blob still counts as "cached" so a missing
/// item doesn't force a re-read (and re-prompt) on every subsequent access.
///
/// Shared across all four categories: previously each category kept its own
/// cache behind its own store; this vault folds them into one blob behind
/// one cache.
static BLOB_CACHE: Mutex<Option<VaultBlob>> = Mutex::new(None);

/// Guards the one-time "which backend did we pick" log line so it fires at
/// most once per process no matter how many times [`SecretVault::open`] is
/// called.
static BACKEND_LOGGED: OnceLock<()> = OnceLock::new();

/// Guards [`migrate_legacy`] so it runs at most once per process no matter
/// how many times [`SecretVault::open`] is called — every facade's `open()`
/// opens this same vault, so without this guard each one would re-probe the
/// three legacy keychain items on every call.
static MIGRATION_DONE: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("data root resolver failed: {0}")]
    DataRoot(#[from] AoError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("keychain error: {0}")]
    Keychain(String),
}

impl From<keyring::Error> for VaultError {
    fn from(e: keyring::Error) -> Self {
        warn_if_interaction_was_suppressed(&e);
        VaultError::Keychain(e.to_string())
    }
}

/// macOS's `errSecInteractionNotAllowed` OSStatus — returned when a keychain
/// operation would have needed to draw a permission prompt, but
/// [`disable_interactive_keychain_prompts`] has told the OS to fail such
/// calls instead of prompting.
#[cfg(target_os = "macos")]
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;

/// Every keychain read/write in this module funnels its error through
/// `?` into this `From` impl, so this is the one place that can recognize
/// "this specific failure is the suppressed-prompt guard doing its job,
/// not a real keychain problem" and say so.
///
/// The `keyring` crate's own macOS backend doesn't give
/// `errSecInteractionNotAllowed` its own [`keyring::Error`] variant — it
/// falls into the catch-all `PlatformFailure`, wrapping the underlying
/// `security_framework::base::Error`. Downcasting to that concrete type and
/// comparing its numeric OSStatus keeps this a comparison against a
/// well-known, stable error code rather than a brittle match against the
/// platform error's `Display` text (which is a human-readable message, not
/// an API contract).
#[cfg(target_os = "macos")]
fn warn_if_interaction_was_suppressed(e: &keyring::Error) {
    let keyring::Error::PlatformFailure(inner) = e else { return };
    let Some(sec_err) = inner.downcast_ref::<security_framework::base::Error>() else { return };
    if sec_err.code() == ERR_SEC_INTERACTION_NOT_ALLOWED {
        tracing::warn!(
            "a keychain secret could not be read or written because interactive keychain \
             prompts are disabled for this process; set LAUNCHPAD_STUDIO_ALLOW_KEYCHAIN_PROMPT=1 \
             to allow the prompt, or set LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK=1 to use the \
             file-backed vault instead"
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn warn_if_interaction_was_suppressed(_e: &keyring::Error) {}

// ---------------------------------------------------------------------------
// Blob
// ---------------------------------------------------------------------------

/// The full contents of the vault, tagged by category rather than flattened
/// into one map, since each category's values have a different shape.
///
/// - `mcp`: keyed by the MCP server key from [`crate::mcp_token_store::derive_server_key`].
/// - `telegram`: keyed by agent id.
/// - `channels`: keyed by `"agent_id:binding_id:secret_role"`.
/// - `providers`: keyed by provider name (`"anthropic"`, `"openai"`,
///   `"gemini"`), value is the API key.
#[derive(Clone, Default, Serialize, Deserialize)]
struct VaultBlob {
    #[serde(default)]
    mcp: BTreeMap<String, McpTokenRecord>,
    #[serde(default)]
    telegram: BTreeMap<String, String>,
    #[serde(default)]
    channels: BTreeMap<String, String>,
    #[serde(default)]
    providers: BTreeMap<String, String>,
}

/// Composes the channel-category map key from a `(agent_id, binding_id,
/// secret_role)` triple. `:` is not a valid character in any of the three
/// components today, so this is unambiguous, though callers only ever build
/// it, never parse it back apart.
fn compose_channel_key(agent_id: &str, binding_id: &str, secret_role: &str) -> String {
    format!("{agent_id}:{binding_id}:{secret_role}")
}

// ---------------------------------------------------------------------------
// Vault
// ---------------------------------------------------------------------------

enum Backend {
    Keychain,
    File(PathBuf),
}

/// Returns `true` when any of the unified or legacy force-file environment
/// variables is set.
fn file_fallback_forced() -> bool {
    std::env::var(FILE_FALLBACK_ENV_VAR).is_ok()
        || LEGACY_FILE_FALLBACK_ENV_VARS.iter().any(|var| std::env::var(var).is_ok())
}

/// Returns `true` when the operator has explicitly asked this process to
/// never touch the OS keychain, or when `CI` marks it as running
/// unattended. Checked independently of [`file_fallback_forced`] so setting
/// either one is sufficient — an operator reaching for either variable name
/// gets the same guarantee.
fn no_keychain_requested() -> bool {
    std::env::var(NO_KEYCHAIN_ENV_VAR).is_ok() || std::env::var("CI").is_ok()
}

/// Returns `true` when the currently running executable is a cargo
/// test/bench harness binary rather than an ordinary build artifact.
///
/// `cfg!(test)` cannot do this job: it reflects whether *this crate* was
/// compiled with `--cfg test`, which is false for a library crate like this
/// one even when the *calling* process is another crate's test binary.
/// Every test/bench harness cargo builds, regardless of which crate it
/// belongs to, is placed at `target/<profile>/deps/<crate>-<16-hex-digest>`
/// — sniffing the running executable's own path for that shape is the one
/// signal that generalizes across crate boundaries. Ordinary `[[bin]]`
/// targets also get an intermediate copy under `deps/`, but the copy
/// anything actually runs (`cargo run`, a shell invocation, a background
/// agent) is the unhashed one at `target/<profile>/<name>` — a false
/// positive here would require deliberately executing cargo's internal
/// deps/ artifact directly, which no normal workflow does.
fn running_from_test_harness() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let in_deps_dir = exe.parent().and_then(Path::file_name).is_some_and(|n| n == "deps");
    if !in_deps_dir {
        return false;
    }
    exe.file_stem().and_then(|s| s.to_str()).is_some_and(|stem| {
        stem.rsplit_once('-')
            .is_some_and(|(_, hash)| hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit()))
    })
}

/// Pre-existing opt-in (this guard did not introduce it) that lets a
/// specific test deliberately exercise the real OS keychain from within a
/// cargo test binary. Every such test is also marked `#[ignore]`, so it
/// never runs as part of the default suite, and self-skips at the top of
/// its body when this isn't set — opting in takes both `--ignored` and this
/// variable. [`running_from_test_harness`]'s auto-detection must defer to
/// it, or the tests that exist specifically to prove real keychain
/// round-tripping would silently start testing the file backend instead.
fn real_keychain_test_opted_in() -> bool {
    std::env::var("LAUNCHPAD_TEST_KEYCHAIN").is_ok()
}

/// Returns `true` when this process must never touch the OS keychain for
/// any reason: an explicit force-file configuration, the kill switch (or
/// `CI`), or a cargo test/bench harness binary that could never hold a
/// stable "Always Allow" keychain grant to begin with (and hasn't been
/// explicitly opted back in via [`real_keychain_test_opted_in`]). This is
/// the gate every keychain-touching code path in this module must check
/// first — [`SecretVault::open`]'s backend selection and
/// [`migrate_legacy`]'s decision whether to probe the three legacy
/// keychain items both use it rather than [`file_fallback_forced`] alone,
/// since either of the other two reasons is just as disqualifying as an
/// explicit force-file config.
fn keychain_forbidden() -> bool {
    let forbidden = file_fallback_forced()
        || no_keychain_requested()
        || (running_from_test_harness() && !real_keychain_test_opted_in());
    if forbidden {
        disable_interactive_keychain_prompts();
    }
    forbidden
}

/// Reifies [`keychain_forbidden`]'s determination into this process's own
/// environment, at most once, so that *every* child process this process
/// ever spawns — from any spawn site, present or future — inherits the
/// decision by ordinary OS environment inheritance instead of each spawn
/// site having to know to pass it down explicitly.
///
/// [`keychain_forbidden`]'s auto-detection (see
/// [`running_from_test_harness`]) is process-local: it can tell a cargo
/// test/bench binary apart from an ordinary one only by inspecting *this*
/// process's own `current_exe()`. A child process this one spawns — a
/// background agent's CLI runner, a tool that shells out, anything started
/// with [`std::process::Command`] — is an ordinary binary from the
/// child's own point of view, so its own call to [`keychain_forbidden`]
/// sees none of the signals that made the parent forbid the keychain and
/// answers `false`. Enumerating every spawn site and passing the decision
/// down by hand is the failure mode this guards against: it silently stops
/// covering the keychain the moment a new spawn site is added and nobody
/// remembers to repeat the wiring. Setting the *environment variable*
/// [`keychain_forbidden`] itself already checks (see
/// [`no_keychain_requested`]) closes that gap structurally instead: once
/// it is present in this process's environment, [`std::process::Command`]
/// inherits it into every child by default, with zero spawn-site-specific
/// code required.
///
/// [`ALLOW_KEYCHAIN_PROMPT_ENV_VAR`] is checked first and wins outright:
/// an operator who has explicitly asked to see a real keychain prompt from
/// *this* process is debugging real keychain behavior, and forcing every
/// child this process spawns off the keychain too would fight that intent.
///
/// # Call this before any other thread can exist
///
/// This crate is edition 2021, where `std::env::set_var` is a safe `fn` —
/// but the underlying hazard the *language* only started enforcing in
/// edition 2024 is present regardless of which edition happens to compile
/// this call: mutating the process environment while another thread might
/// concurrently read it (`std::env::var`, `getenv` through any FFI
/// boundary, …) is a data race on every platform's C library, since none
/// of them guard `environ` with a lock. This function does not — cannot —
/// enforce that no other thread exists; it only guarantees the *write*
/// itself happens at most once via [`OnceLock`]. Safety depends entirely on
/// the caller invoking it before it could be true, i.e. as the first
/// statement of `main`, before constructing any multi-threaded async
/// runtime. A `#[tokio::main] async fn main()` does not satisfy this: the
/// runtime the macro builds spawns its worker threads before the body of
/// that `async fn` — including a call at its very first line — ever runs.
/// Both binaries that call this today (`ao-server`, `ao-engine-tools-cli`)
/// use a plain synchronous `fn main` for exactly this reason, calling this
/// function first and only afterward building a runtime by hand and
/// calling [`tokio::runtime::Runtime::block_on`]. Any future caller must do
/// the same — call this before any runtime (tokio or otherwise) exists,
/// not from inside one.
pub fn propagate_keychain_forbidden() {
    static PROPAGATED: OnceLock<()> = OnceLock::new();
    PROPAGATED.get_or_init(propagate_keychain_forbidden_once);
}

/// The decision-and-write logic behind [`propagate_keychain_forbidden`],
/// split out so a test can exercise it directly and repeatedly. The public
/// function's [`OnceLock`] is a genuine "at most once for the life of this
/// process" guard — appropriate for `main`, useless for a test that needs
/// to observe the decision under more than one env-var combination within
/// one test binary process. This function has no such guard: calling it
/// twice just re-runs the same (idempotent — see the `NO_KEYCHAIN_ENV_VAR`
/// already-set check below) decision, which is exactly what a unit test
/// needs and exactly what `main` does not (hence the wrapper above).
fn propagate_keychain_forbidden_once() {
    if std::env::var(ALLOW_KEYCHAIN_PROMPT_ENV_VAR).is_ok() {
        return;
    }
    if keychain_forbidden() && std::env::var(NO_KEYCHAIN_ENV_VAR).is_err() {
        std::env::set_var(NO_KEYCHAIN_ENV_VAR, "1");
    }
}

/// Decides whether the server should call
/// [`disable_interactive_keychain_prompts`] at startup, in this precedence
/// order:
///
/// 1. [`ALLOW_KEYCHAIN_PROMPT_ENV_VAR`] is set → never suppress. The escape
///    hatch always wins, so a developer can opt back into a real prompt
///    without a code change.
/// 2. [`keychain_forbidden`] is `true` → suppress. Whatever makes the
///    keychain off-limits for this process at all (an explicit force-file
///    config, the kill switch, a test/bench harness binary) applies just as
///    much to whether a prompt should ever be allowed to draw.
/// 3. Otherwise → do not suppress, whatever the build profile. A prompt that
///    reaches this point is a legitimate one and should be allowed to draw
///    for whichever user is present.
///
/// The build profile is deliberately *not* consulted. An earlier revision
/// suppressed for any `cfg!(debug_assertions)` build, but "debug build" was
/// only ever a proxy for "probably a non-interactive dev/test process".
/// [`keychain_forbidden`] plus its propagation to child processes (see
/// [`propagate_keychain_forbidden`]) supplies that real signal directly, so
/// the proxy is now both redundant and harmful: a developer interactively
/// running a debug server genuinely wants the prompt, and suppressing it
/// turned every keychain read into an `errSecAuthFailed` failure with no
/// popup ever shown.
///
/// The tools CLI does not consult this — it suppresses unconditionally,
/// since it is headless by nature and predominantly agent-invoked, with no
/// user ever present to answer a prompt regardless of build profile.
pub fn should_suppress_keychain_prompts() -> bool {
    if std::env::var(ALLOW_KEYCHAIN_PROMPT_ENV_VAR).is_ok() {
        return false;
    }
    keychain_forbidden()
}

fn log_selected_backend_once(backend: &Backend) {
    BACKEND_LOGGED.get_or_init(|| match backend {
        Backend::Keychain => tracing::info!("secret vault backend: keychain"),
        Backend::File(path) => tracing::info!("secret vault backend: file ({})", path.display()),
    });
}

/// Consolidated secret store for MCP OAuth tokens, Telegram bot tokens,
/// channel-binding secrets, and provider API keys.
///
/// Open with [`SecretVault::open`]. The backing store (keychain or file) is
/// chosen once and hidden from callers. All operations are synchronous and
/// never panic — a failure to reach the backend is reported as `Err`, and
/// the caller decides how to degrade.
pub struct SecretVault {
    backend: Backend,
}

impl SecretVault {
    /// Open the vault using the best available backend.
    ///
    /// Never panics: a keychain probe failure or a data-root resolution
    /// failure is reported as `Err`, not a panic — callers decide whether
    /// that is fatal for their use case.
    pub fn open() -> Result<Self, VaultError> {
        let root = resolve_data_root()?;
        let file_path = root.join(FILE_STORE_NAME);

        let backend = if keychain_forbidden() {
            Backend::File(file_path)
        } else if keychain_available() {
            Backend::Keychain
        } else {
            Backend::File(file_path)
        };

        log_selected_backend_once(&backend);
        migrate_legacy_once(&backend, &root);
        Ok(Self { backend })
    }

    /// Create a file-backed vault rooted at `dir`, never touching the OS
    /// keychain.
    ///
    /// The vault file will be written as `<dir>/secret_vault.json`. Useful in
    /// tests that need an isolated vault without touching the OS keychain or
    /// the user's real data root.
    pub fn new_with_file_fallback(dir: PathBuf) -> Self {
        Self { backend: Backend::File(dir.join(FILE_STORE_NAME)) }
    }

    // --- MCP ---

    pub fn get_mcp(&self, server_key: &str) -> Result<Option<McpTokenRecord>, VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_get_mcp(server_key),
            Backend::File(path) => file_get_mcp(path, server_key),
        }
    }

    pub fn set_mcp(&self, server_key: &str, record: &McpTokenRecord) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_set_mcp(server_key, record),
            Backend::File(path) => file_set_mcp(path, server_key, record),
        }
    }

    pub fn delete_mcp(&self, server_key: &str) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_delete_mcp(server_key),
            Backend::File(path) => file_delete_mcp(path, server_key),
        }
    }

    /// Return the current MCP credential for `server_key`, bypassing the
    /// keychain-backed process cache and re-reading the backing store first.
    ///
    /// Use this instead of [`Self::get_mcp`] on any path that decides
    /// whether to use a refresh token, or that is about to send one to a
    /// provider — the plain cached [`Self::get_mcp`] can return a copy of
    /// the credential that predates a rotation performed by another process
    /// sharing this same keychain item. Sending an already-rotated-out
    /// refresh token is exactly what triggers reuse detection on providers
    /// that rotate every use, and revokes the whole grant — see this
    /// module's docs and [`crate::mcp_token_store`].
    ///
    /// The file backend has no such cache (every read already hits disk),
    /// so this is identical to [`Self::get_mcp`] there.
    pub fn get_mcp_fresh(&self, server_key: &str) -> Result<Option<McpTokenRecord>, VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_get_mcp_fresh(server_key),
            Backend::File(path) => file_get_mcp(path, server_key),
        }
    }

    // --- Telegram ---

    pub fn get_telegram(&self, agent_id: &str) -> Result<Option<String>, VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_get_telegram(agent_id),
            Backend::File(path) => file_get_telegram(path, agent_id),
        }
    }

    pub fn set_telegram(&self, agent_id: &str, token: &str) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_set_telegram(agent_id, token),
            Backend::File(path) => file_set_telegram(path, agent_id, token),
        }
    }

    pub fn delete_telegram(&self, agent_id: &str) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_delete_telegram(agent_id),
            Backend::File(path) => file_delete_telegram(path, agent_id),
        }
    }

    // --- Channels ---

    pub fn get_channel(
        &self,
        agent_id: &str,
        binding_id: &str,
        secret_role: &str,
    ) -> Result<Option<String>, VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_get_channel(agent_id, binding_id, secret_role),
            Backend::File(path) => file_get_channel(path, agent_id, binding_id, secret_role),
        }
    }

    pub fn set_channel(
        &self,
        agent_id: &str,
        binding_id: &str,
        secret_role: &str,
        secret: &str,
    ) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_set_channel(agent_id, binding_id, secret_role, secret),
            Backend::File(path) => file_set_channel(path, agent_id, binding_id, secret_role, secret),
        }
    }

    pub fn delete_channel(&self, agent_id: &str, binding_id: &str, secret_role: &str) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_delete_channel(agent_id, binding_id, secret_role),
            Backend::File(path) => file_delete_channel(path, agent_id, binding_id, secret_role),
        }
    }

    // --- Providers ---

    pub fn get_provider(&self, provider: &str) -> Result<Option<String>, VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_get_provider(provider),
            Backend::File(path) => file_get_provider(path, provider),
        }
    }

    pub fn set_provider(&self, provider: &str, api_key: &str) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_set_provider(provider, api_key),
            Backend::File(path) => file_set_provider(path, provider, api_key),
        }
    }

    pub fn delete_provider(&self, provider: &str) -> Result<(), VaultError> {
        match &self.backend {
            Backend::Keychain => keychain_delete_provider(provider),
            Backend::File(path) => file_delete_provider(path, provider),
        }
    }

    /// True when this vault is backed by the OS keychain rather than the
    /// `0o600` JSON file fallback.
    ///
    /// Callers that also keep a plaintext copy of a secret somewhere else
    /// (e.g. a legacy field in another config file) use this to decide
    /// whether it's safe to scrub that copy once the vault holds the
    /// authoritative value: the keychain backend removes the plaintext
    /// entirely, while the file backend is itself just a file, so scrubbing
    /// elsewhere would only relocate the plaintext rather than remove it.
    pub fn is_keychain_backed(&self) -> bool {
        matches!(self.backend, Backend::Keychain)
    }
}

// ---------------------------------------------------------------------------
// Keychain backend
// ---------------------------------------------------------------------------

/// Asks the OS keychain to fail any interactive-authorization request
/// immediately (`errSecInteractionNotAllowed`) instead of drawing a GUI
/// prompt, for the rest of this process's life. A structural guarantee
/// rather than a policy one: it holds even if some future code path ends up
/// constructing a [`keyring::Entry`] directly instead of going through
/// [`SecretVault`].
///
/// Idempotent and safe to call from more than one place — the underlying
/// macOS call only runs once per process via an internal guard. On any
/// platform other than macOS this is a no-op: [`keyring`]'s other native
/// backends (Windows Credential Manager, Linux Secret Service) don't share
/// this failure mode the way the macOS `SecKeychain*` API family does.
///
/// The tools CLI calls this unconditionally at startup, before touching the
/// vault — it is headless by nature and predominantly agent-invoked, so
/// there is never a legitimate reason to let a prompt draw. The server
/// instead calls this only when [`should_suppress_keychain_prompts`]
/// returns `true`; see that function's docs for why a release server build
/// is treated differently from a dev build. [`SecretVault::open`] also
/// calls it itself whenever it independently determines the keychain is
/// off-limits (see [`keychain_forbidden`]), so that guarantee holds even
/// for a process that never opted in explicitly. The Tauri desktop app
/// must NOT call this — prompting its user for one-time keychain
/// authorization is the legitimate, intended flow there.
pub fn disable_interactive_keychain_prompts() {
    #[cfg(target_os = "macos")]
    {
        static DISABLED: OnceLock<()> = OnceLock::new();
        DISABLED.get_or_init(|| {
            match security_framework::os::macos::keychain::SecKeychain::disable_user_interaction() {
                Ok(lock) => {
                    // The returned guard re-enables interactive prompts on
                    // drop. This process must never draw one again for the
                    // rest of its life, so the guard is deliberately leaked
                    // rather than held or dropped.
                    std::mem::forget(lock);
                }
                Err(e) => {
                    tracing::warn!("failed to disable macOS keychain user-interaction prompts: {e}");
                }
            }
        });
    }
}

/// Returns `true` when the OS keychain subsystem is reachable.
///
/// Probes by reading a non-existent entry — `NoEntry` means the keychain
/// replied, any other error means it's unavailable.
fn keychain_available() -> bool {
    match keyring::Entry::new(KEYRING_SERVICE, "_probe") {
        Ok(entry) => matches!(entry.get_password(), Ok(_) | Err(keyring::Error::NoEntry)),
        Err(_) => false,
    }
}

fn keychain_entry() -> Result<keyring::Entry, VaultError> {
    keyring::Entry::new(KEYRING_SERVICE, CONSOLIDATED_ACCOUNT).map_err(Into::into)
}

/// Reads the consolidated keychain item and parses it into the full vault
/// blob. A missing item or a blob that fails to parse is treated as an empty
/// vault rather than an error, so a corrupted or not-yet-created item never
/// blocks reads.
///
/// This collapses "the vault item doesn't exist yet" and "the blob is
/// corrupt" into the same empty-blob result, but that is safe here: the
/// per-category accessors (e.g. [`SecretVault::get_mcp`]) already return
/// `Option`, so "no entry for this key" reads identically whichever of the
/// two caused it. Callers that need to distinguish "never had a credential"
/// from "credential lookup failed" must do so one layer up, from the
/// `Option` a category accessor returns versus the `Err` it can also return
/// — not from anything at this level.
fn read_blob() -> Result<VaultBlob, VaultError> {
    match keychain_entry()?.get_password() {
        Ok(json) => match serde_json::from_str(&json) {
            Ok(blob) => Ok(blob),
            Err(e) => {
                tracing::warn!("consolidated secret vault blob failed to parse, treating as empty: {e}");
                Ok(VaultBlob::default())
            }
        },
        Err(keyring::Error::NoEntry) => Ok(VaultBlob::default()),
        Err(e) => Err(e.into()),
    }
}

fn write_blob(blob: &VaultBlob) -> Result<(), VaultError> {
    let json = serde_json::to_string(blob)?;
    keychain_entry()?.set_password(&json).map_err(Into::into)
}

/// Populates `cache` via `read` only when empty — the standard cached-read
/// contract used by every category except the MCP refresh-decision path
/// (see [`force_repopulate_with`]). Parametrized over `read` rather than
/// calling [`read_blob`] directly so this contract — "populate once, then
/// trust the cache for the rest of the process" — is unit-testable without
/// touching the real OS keychain (this crate's tests must never do that;
/// see the `apple-native`/`windows-native`/`linux-native-sync-persistent`
/// comment on the workspace `keyring` dependency).
///
/// Must be called while holding `BLOB_CACHE`'s lock so concurrent first
/// callers collapse into a single keychain read instead of racing to
/// populate it independently.
fn ensure_populated_with(
    cache: &mut Option<VaultBlob>,
    read: impl FnOnce() -> Result<VaultBlob, VaultError>,
) -> Result<(), VaultError> {
    if cache.is_none() {
        *cache = Some(read()?);
    }
    Ok(())
}

/// Unconditionally repopulates `cache` from `read`, discarding whatever was
/// cached before — the counterpart to [`ensure_populated_with`] for callers
/// that know a stale process-lifetime cache is unacceptable.
///
/// The MCP refresh-decision path ([`SecretVault::get_mcp_fresh`]) is the one
/// caller: another process sharing this same keychain item may have already
/// rotated a refresh token since this process's cache was last populated,
/// and presenting an already-rotated-out refresh token to a provider that
/// rotates on every use (Notion among them) is exactly what triggers reuse
/// detection and revokes the whole grant.
fn force_repopulate_with(
    cache: &mut Option<VaultBlob>,
    read: impl FnOnce() -> Result<VaultBlob, VaultError>,
) -> Result<(), VaultError> {
    *cache = Some(read()?);
    Ok(())
}

fn ensure_populated(cache: &mut Option<VaultBlob>) -> Result<(), VaultError> {
    ensure_populated_with(cache, read_blob)
}

/// Reads one field out of the cached blob, populating the cache first if
/// this is the first access this process.
fn keychain_get_field<T>(field: impl FnOnce(&VaultBlob) -> T) -> Result<T, VaultError> {
    let mut guard = BLOB_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    ensure_populated(&mut guard)?;
    Ok(field(guard.as_ref().expect("populated above")))
}

/// Like [`keychain_get_field`] but always re-reads the keychain item first,
/// bypassing whatever is cached.
///
/// Only [`SecretVault::get_mcp_fresh`] uses this. Every other read (provider
/// API keys, telegram tokens, channel secrets, and every other MCP read)
/// stays on the cheap cached path in [`keychain_get_field`]: provider keys
/// in particular are read on every agent/subagent build — far more often
/// than an OAuth token is ever refreshed — so putting them on the re-read
/// path would trade a real hot-path regression for a freshness guarantee
/// they don't need (a user-entered API key doesn't rotate out from under a
/// running process the way a provider-rotated refresh token does).
fn keychain_get_field_fresh<T>(field: impl FnOnce(&VaultBlob) -> T) -> Result<T, VaultError> {
    let mut guard = BLOB_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    force_repopulate_with(&mut guard, read_blob)?;
    Ok(field(guard.as_ref().expect("populated above")))
}

fn keychain_get_mcp_fresh(server_key: &str) -> Result<Option<McpTokenRecord>, VaultError> {
    keychain_get_field_fresh(|blob| blob.mcp.get(server_key).cloned())
}

/// Applies `mutate` to a clone of the cached blob, writes the result back to
/// the keychain, then swaps it into the cache. Populates the cache first if
/// this is the first access this process.
fn keychain_mutate(mutate: impl FnOnce(&mut VaultBlob)) -> Result<(), VaultError> {
    let mut guard = BLOB_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    ensure_populated(&mut guard)?;

    let mut blob = guard.as_ref().expect("populated above").clone();
    mutate(&mut blob);
    write_blob(&blob)?;
    *guard = Some(blob);
    Ok(())
}

fn keychain_get_mcp(server_key: &str) -> Result<Option<McpTokenRecord>, VaultError> {
    keychain_get_field(|blob| blob.mcp.get(server_key).cloned())
}

fn keychain_set_mcp(server_key: &str, record: &McpTokenRecord) -> Result<(), VaultError> {
    keychain_mutate(|blob| {
        blob.mcp.insert(server_key.to_owned(), record.clone());
    })
}

fn keychain_delete_mcp(server_key: &str) -> Result<(), VaultError> {
    keychain_mutate(|blob| {
        blob.mcp.remove(server_key);
    })
}

fn keychain_get_telegram(agent_id: &str) -> Result<Option<String>, VaultError> {
    keychain_get_field(|blob| blob.telegram.get(agent_id).cloned())
}

fn keychain_set_telegram(agent_id: &str, token: &str) -> Result<(), VaultError> {
    keychain_mutate(|blob| {
        blob.telegram.insert(agent_id.to_owned(), token.to_owned());
    })
}

fn keychain_delete_telegram(agent_id: &str) -> Result<(), VaultError> {
    keychain_mutate(|blob| {
        blob.telegram.remove(agent_id);
    })
}

fn keychain_get_channel(agent_id: &str, binding_id: &str, secret_role: &str) -> Result<Option<String>, VaultError> {
    let key = compose_channel_key(agent_id, binding_id, secret_role);
    keychain_get_field(|blob| blob.channels.get(&key).cloned())
}

fn keychain_set_channel(
    agent_id: &str,
    binding_id: &str,
    secret_role: &str,
    secret: &str,
) -> Result<(), VaultError> {
    let key = compose_channel_key(agent_id, binding_id, secret_role);
    keychain_mutate(|blob| {
        blob.channels.insert(key, secret.to_owned());
    })
}

fn keychain_delete_channel(agent_id: &str, binding_id: &str, secret_role: &str) -> Result<(), VaultError> {
    let key = compose_channel_key(agent_id, binding_id, secret_role);
    keychain_mutate(|blob| {
        blob.channels.remove(&key);
    })
}

fn keychain_get_provider(provider: &str) -> Result<Option<String>, VaultError> {
    keychain_get_field(|blob| blob.providers.get(provider).cloned())
}

fn keychain_set_provider(provider: &str, api_key: &str) -> Result<(), VaultError> {
    keychain_mutate(|blob| {
        blob.providers.insert(provider.to_owned(), api_key.to_owned());
    })
}

fn keychain_delete_provider(provider: &str) -> Result<(), VaultError> {
    keychain_mutate(|blob| {
        blob.providers.remove(provider);
    })
}

// ---------------------------------------------------------------------------
// File backend
// ---------------------------------------------------------------------------

fn file_load(path: &PathBuf) -> Result<VaultBlob, VaultError> {
    if !path.exists() {
        return Ok(VaultBlob::default());
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

fn file_save(path: &PathBuf, blob: &VaultBlob) -> Result<(), VaultError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(blob)?;
    std::fs::write(path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn file_get_field<T>(path: &PathBuf, field: impl FnOnce(&VaultBlob) -> T) -> Result<T, VaultError> {
    let blob = file_load(path)?;
    Ok(field(&blob))
}

fn file_mutate(path: &PathBuf, mutate: impl FnOnce(&mut VaultBlob)) -> Result<(), VaultError> {
    let mut blob = file_load(path)?;
    mutate(&mut blob);
    file_save(path, &blob)
}

fn file_get_mcp(path: &PathBuf, server_key: &str) -> Result<Option<McpTokenRecord>, VaultError> {
    file_get_field(path, |blob| blob.mcp.get(server_key).cloned())
}

fn file_set_mcp(path: &PathBuf, server_key: &str, record: &McpTokenRecord) -> Result<(), VaultError> {
    file_mutate(path, |blob| {
        blob.mcp.insert(server_key.to_owned(), record.clone());
    })
}

fn file_delete_mcp(path: &PathBuf, server_key: &str) -> Result<(), VaultError> {
    file_mutate(path, |blob| {
        blob.mcp.remove(server_key);
    })
}

fn file_get_telegram(path: &PathBuf, agent_id: &str) -> Result<Option<String>, VaultError> {
    file_get_field(path, |blob| blob.telegram.get(agent_id).cloned())
}

fn file_set_telegram(path: &PathBuf, agent_id: &str, token: &str) -> Result<(), VaultError> {
    file_mutate(path, |blob| {
        blob.telegram.insert(agent_id.to_owned(), token.to_owned());
    })
}

fn file_delete_telegram(path: &PathBuf, agent_id: &str) -> Result<(), VaultError> {
    file_mutate(path, |blob| {
        blob.telegram.remove(agent_id);
    })
}

fn file_get_channel(
    path: &PathBuf,
    agent_id: &str,
    binding_id: &str,
    secret_role: &str,
) -> Result<Option<String>, VaultError> {
    let key = compose_channel_key(agent_id, binding_id, secret_role);
    file_get_field(path, |blob| blob.channels.get(&key).cloned())
}

fn file_set_channel(
    path: &PathBuf,
    agent_id: &str,
    binding_id: &str,
    secret_role: &str,
    secret: &str,
) -> Result<(), VaultError> {
    let key = compose_channel_key(agent_id, binding_id, secret_role);
    file_mutate(path, |blob| {
        blob.channels.insert(key, secret.to_owned());
    })
}

fn file_delete_channel(path: &PathBuf, agent_id: &str, binding_id: &str, secret_role: &str) -> Result<(), VaultError> {
    let key = compose_channel_key(agent_id, binding_id, secret_role);
    file_mutate(path, |blob| {
        blob.channels.remove(&key);
    })
}

fn file_get_provider(path: &PathBuf, provider: &str) -> Result<Option<String>, VaultError> {
    file_get_field(path, |blob| blob.providers.get(provider).cloned())
}

fn file_set_provider(path: &PathBuf, provider: &str, api_key: &str) -> Result<(), VaultError> {
    file_mutate(path, |blob| {
        blob.providers.insert(provider.to_owned(), api_key.to_owned());
    })
}

fn file_delete_provider(path: &PathBuf, provider: &str) -> Result<(), VaultError> {
    file_mutate(path, |blob| {
        blob.providers.remove(provider);
    })
}

// ---------------------------------------------------------------------------
// Legacy migration
// ---------------------------------------------------------------------------
//
// Before this vault existed, `McpTokenStore`, `TelegramTokenStore`, and
// `ChannelSecretStore` each owned their own consolidated keychain item (one
// per category) plus their own JSON file fallback. Now that they are thin
// facades over this vault, whatever they'd already stored under those old
// locations needs to be folded in exactly once so upgrading doesn't orphan a
// user's existing tokens. This only handles each store's already-consolidated
// item/file — not the deeper per-server-key items `McpTokenStore` itself used
// to lazily fold on read, since every store had already migrated onto its own
// consolidated blob well before this vault was introduced.

fn read_legacy_keychain_blob<T: for<'de> Deserialize<'de>>(service: &str, account: &str) -> Option<T> {
    let entry = keyring::Entry::new(service, account).ok()?;
    let json = entry.get_password().ok()?;
    serde_json::from_str(&json).ok()
}

fn delete_legacy_keychain_item(service: &str, account: &str) {
    if let Ok(entry) = keyring::Entry::new(service, account) {
        let _ = entry.delete_credential();
    }
}

fn read_legacy_file_blob<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn remove_legacy_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Runs [`migrate_legacy`] at most once per process, logging and swallowing
/// any failure rather than propagating it — a migration hiccup must not turn
/// an otherwise-successful [`SecretVault::open`] into a failure.
fn migrate_legacy_once(backend: &Backend, root: &Path) {
    if MIGRATION_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(e) = migrate_legacy(backend, root) {
        tracing::warn!("legacy secret migration failed; pre-existing stored secrets may need to be re-entered: {e}");
    }
}

/// Folds the three legacy consolidated keychain items and the three legacy
/// fallback files into this vault's blob, then deletes the legacy sources.
///
/// Idempotent by construction rather than by the [`MIGRATION_DONE`] guard
/// alone: once a legacy item/file is folded in it is deleted, so a second
/// call (e.g. from a different data root sharing the same OS keychain
/// service names) finds nothing left to migrate and performs no writes.
/// Existing vault entries are never overwritten by a legacy value — `or_insert`
/// keeps whatever is already in the vault should this ever run twice with
/// unmigrated data still present.
fn migrate_legacy(backend: &Backend, root: &Path) -> Result<(), VaultError> {
    // Anything that keeps this process off the keychain entirely (an
    // explicit force-file config, the kill switch, or a test/bench harness
    // binary) applies just as much to the three legacy items as it does to
    // the consolidated one — migration must not be the one code path that
    // still reaches the keychain when everything else has been told not to.
    let probe_keychain = !keychain_forbidden();

    let mut mcp: BTreeMap<String, McpTokenRecord> = BTreeMap::new();
    let mut telegram: BTreeMap<String, String> = BTreeMap::new();
    let mut channels: BTreeMap<String, String> = BTreeMap::new();

    if probe_keychain {
        if let Some(blob) = read_legacy_keychain_blob::<BTreeMap<String, McpTokenRecord>>(
            LEGACY_MCP_KEYRING_SERVICE,
            LEGACY_MCP_CONSOLIDATED_ACCOUNT,
        ) {
            mcp.extend(blob);
        }
        if let Some(blob) = read_legacy_keychain_blob::<BTreeMap<String, String>>(
            LEGACY_TELEGRAM_KEYRING_SERVICE,
            LEGACY_TELEGRAM_CONSOLIDATED_ACCOUNT,
        ) {
            telegram.extend(blob);
        }
        if let Some(blob) = read_legacy_keychain_blob::<BTreeMap<String, String>>(
            LEGACY_CHANNEL_KEYRING_SERVICE,
            LEGACY_CHANNEL_CONSOLIDATED_ACCOUNT,
        ) {
            channels.extend(blob);
        }
    }

    let mcp_file = root.join(LEGACY_MCP_FILE_NAME);
    let telegram_file = root.join(LEGACY_TELEGRAM_FILE_NAME);
    let channels_file = root.join(LEGACY_CHANNEL_FILE_NAME);

    if let Some(blob) = read_legacy_file_blob::<BTreeMap<String, McpTokenRecord>>(&mcp_file) {
        mcp.extend(blob);
    }
    if let Some(blob) = read_legacy_file_blob::<BTreeMap<String, String>>(&telegram_file) {
        telegram.extend(blob);
    }
    if let Some(blob) = read_legacy_file_blob::<BTreeMap<String, String>>(&channels_file) {
        channels.extend(blob);
    }

    if mcp.is_empty() && telegram.is_empty() && channels.is_empty() {
        return Ok(());
    }

    let fold = move |blob: &mut VaultBlob| {
        for (k, v) in mcp {
            blob.mcp.entry(k).or_insert(v);
        }
        for (k, v) in telegram {
            blob.telegram.entry(k).or_insert(v);
        }
        for (k, v) in channels {
            blob.channels.entry(k).or_insert(v);
        }
    };
    match backend {
        Backend::Keychain => keychain_mutate(fold)?,
        Backend::File(path) => file_mutate(path, fold)?,
    }

    if probe_keychain {
        delete_legacy_keychain_item(LEGACY_MCP_KEYRING_SERVICE, LEGACY_MCP_CONSOLIDATED_ACCOUNT);
        delete_legacy_keychain_item(LEGACY_TELEGRAM_KEYRING_SERVICE, LEGACY_TELEGRAM_CONSOLIDATED_ACCOUNT);
        delete_legacy_keychain_item(LEGACY_CHANNEL_KEYRING_SERVICE, LEGACY_CHANNEL_CONSOLIDATED_ACCOUNT);
    }
    remove_legacy_file(&mcp_file);
    remove_legacy_file(&telegram_file);
    remove_legacy_file(&channels_file);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{lock_env, EnvGuard};
    use ao_protocol::data_root::DATA_DIR_ENV_VAR;
    use chrono::{Duration, Utc};

    fn sample_mcp_record() -> McpTokenRecord {
        McpTokenRecord {
            access_token: "at_test_value".to_owned(),
            refresh_token: Some("rt_test_value".to_owned()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            scope: Some("read write".to_owned()),
            client_id: "client_abc".to_owned(),
            client_secret: Some("cs_test_value".to_owned()),
            token_endpoint: Some("https://auth.example.com/token".to_owned()),
        }
    }

    /// Points the data root at a fresh tempdir and forces the file backend.
    /// Returns the tempdir (kept alive for the caller's `_dir` binding) plus
    /// both env guards so they stay in scope for the duration of the test.
    fn set_up() -> (tempfile::TempDir, EnvGuard, EnvGuard) {
        let dir = tempfile::tempdir().expect("tempdir");
        let dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let fb = EnvGuard::set(FILE_FALLBACK_ENV_VAR, "1");
        (dir, dd, fb)
    }

    // --- Round trip per category ---

    #[test]
    fn mcp_round_trip() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();
        let vault = SecretVault::open().expect("open");
        let key = "server_a";

        assert!(vault.get_mcp(key).expect("initial get").is_none());

        let record = sample_mcp_record();
        vault.set_mcp(key, &record).expect("set");
        let got = vault.get_mcp(key).expect("get after set").expect("present");
        assert_eq!(got.access_token, record.access_token);
        assert_eq!(got.refresh_token, record.refresh_token);

        vault.delete_mcp(key).expect("delete");
        assert!(vault.get_mcp(key).expect("get after delete").is_none());
    }

    #[test]
    fn telegram_round_trip() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();
        let vault = SecretVault::open().expect("open");
        let agent_id = "agent-a";

        assert!(vault.get_telegram(agent_id).expect("initial get").is_none());

        vault.set_telegram(agent_id, "bot-token-value").expect("set");
        assert_eq!(vault.get_telegram(agent_id).expect("get after set"), Some("bot-token-value".to_owned()));

        vault.delete_telegram(agent_id).expect("delete");
        assert!(vault.get_telegram(agent_id).expect("get after delete").is_none());
    }

    #[test]
    fn channel_round_trip() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();
        let vault = SecretVault::open().expect("open");

        assert!(vault.get_channel("agent-a", "email", "password").expect("initial get").is_none());

        vault.set_channel("agent-a", "email", "password", "hunter2").expect("set");
        assert_eq!(
            vault.get_channel("agent-a", "email", "password").expect("get after set"),
            Some("hunter2".to_owned())
        );

        vault.delete_channel("agent-a", "email", "password").expect("delete");
        assert!(vault.get_channel("agent-a", "email", "password").expect("get after delete").is_none());
    }

    #[test]
    fn provider_round_trip() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();
        let vault = SecretVault::open().expect("open");

        assert!(vault.get_provider("anthropic").expect("initial get").is_none());

        vault.set_provider("anthropic", "sk-test-key").expect("set");
        assert_eq!(vault.get_provider("anthropic").expect("get after set"), Some("sk-test-key".to_owned()));

        vault.delete_provider("anthropic").expect("delete");
        assert!(vault.get_provider("anthropic").expect("get after delete").is_none());
    }

    #[test]
    fn delete_nonexistent_key_is_noop_for_every_category() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();
        let vault = SecretVault::open().expect("open");

        vault.delete_mcp("no-such-key").expect("delete mcp must not error");
        vault.delete_telegram("no-such-agent").expect("delete telegram must not error");
        vault.delete_channel("no-such-agent", "email", "password").expect("delete channel must not error");
        vault.delete_provider("no-such-provider").expect("delete provider must not error");
    }

    /// Proves the reason the blob is tagged by category instead of one flat
    /// map: the same string used as a key in two unrelated categories must
    /// not let a write to one category clobber the other.
    #[test]
    fn categories_do_not_collide_on_shared_keys() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();
        let vault = SecretVault::open().expect("open");

        vault.set_telegram("shared", "telegram-value").expect("set telegram");
        vault.set_provider("shared", "provider-value").expect("set provider");

        assert_eq!(vault.get_telegram("shared").unwrap(), Some("telegram-value".to_owned()));
        assert_eq!(vault.get_provider("shared").unwrap(), Some("provider-value".to_owned()));
    }

    // --- Cache invalidation contract (the mechanism behind get_mcp_fresh) ---
    //
    // These test the cache/re-read MECHANISM directly against a fake `read`
    // closure rather than the real keychain: this crate's tests must never
    // touch the real OS keychain (see the workspace `keyring` dependency
    // comment), and the keychain is the only backend where this cache
    // exists at all — the file backend re-reads on every call already, so
    // there is nothing to prove staleness with there. `get_mcp_fresh`'s
    // live-path contract is covered separately below via the file backend.

    #[test]
    fn ensure_populated_with_does_not_reread_an_already_populated_cache() {
        let mut cache: Option<VaultBlob> = Some(VaultBlob::default());
        let mut reads = 0;
        ensure_populated_with(&mut cache, || {
            reads += 1;
            Ok(VaultBlob::default())
        })
        .expect("ok");
        assert_eq!(
            reads, 0,
            "an already-populated cache must not be re-read — this is the staleness the fix targets"
        );
    }

    #[test]
    fn ensure_populated_with_reads_once_when_cache_is_empty() {
        let mut cache: Option<VaultBlob> = None;
        let mut reads = 0;
        ensure_populated_with(&mut cache, || {
            reads += 1;
            Ok(VaultBlob::default())
        })
        .expect("ok");
        assert_eq!(reads, 1);
        assert!(cache.is_some());
    }

    #[test]
    fn force_repopulate_with_rereads_even_when_already_cached() {
        let mut stale = VaultBlob::default();
        stale.mcp.insert("k".to_owned(), sample_mcp_record());
        let mut cache: Option<VaultBlob> = Some(stale);

        let mut fresh_record = sample_mcp_record();
        fresh_record.access_token = "at_rotated".to_owned();
        let mut fresh = VaultBlob::default();
        fresh.mcp.insert("k".to_owned(), fresh_record);

        let mut reads = 0;
        force_repopulate_with(&mut cache, || {
            reads += 1;
            Ok(fresh)
        })
        .expect("ok");

        assert_eq!(reads, 1, "force_repopulate_with must always re-read regardless of cache state");
        assert_eq!(
            cache.as_ref().unwrap().mcp.get("k").unwrap().access_token,
            "at_rotated",
            "the forced read must replace the stale cached value"
        );
    }

    // --- get_mcp_fresh live-path contract ---

    #[test]
    fn get_mcp_fresh_observes_a_write_made_by_a_second_vault_handle() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();

        let writer = SecretVault::open().expect("open writer");
        let reader = SecretVault::open().expect("open reader");

        writer.set_mcp("server_a", &sample_mcp_record()).expect("write from a second handle");

        let got = reader.get_mcp_fresh("server_a").expect("get_mcp_fresh").expect("present");
        assert_eq!(got.access_token, sample_mcp_record().access_token);
    }

    // --- Persistence across reopen ---

    #[test]
    fn blob_persists_across_reopen() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();

        let vault = SecretVault::open().expect("open");
        vault.set_mcp("server_a", &sample_mcp_record()).expect("set mcp");
        vault.set_telegram("agent-a", "bot-token-value").expect("set telegram");
        vault.set_channel("agent-a", "email", "password", "hunter2").expect("set channel");
        vault.set_provider("anthropic", "sk-test-key").expect("set provider");
        drop(vault);

        let reopened = SecretVault::open().expect("reopen");
        assert!(reopened.get_mcp("server_a").expect("get mcp").is_some());
        assert_eq!(reopened.get_telegram("agent-a").expect("get telegram"), Some("bot-token-value".to_owned()));
        assert_eq!(
            reopened.get_channel("agent-a", "email", "password").expect("get channel"),
            Some("hunter2".to_owned())
        );
        assert_eq!(reopened.get_provider("anthropic").expect("get provider"), Some("sk-test-key".to_owned()));
    }

    // --- Legacy migration ---

    #[test]
    fn migration_folds_legacy_fallback_files_and_deletes_them() {
        let _lock = lock_env();
        MIGRATION_DONE.store(false, Ordering::SeqCst);
        let (dir, _dd, _fb) = set_up();

        let mcp_record = sample_mcp_record();
        let mut mcp_seed = BTreeMap::new();
        mcp_seed.insert("server_a".to_owned(), mcp_record.clone());
        std::fs::write(dir.path().join(LEGACY_MCP_FILE_NAME), serde_json::to_string(&mcp_seed).unwrap())
            .expect("seed legacy mcp file");

        let mut telegram_seed = BTreeMap::new();
        telegram_seed.insert("agent-a".to_owned(), "legacy-bot-token".to_owned());
        std::fs::write(dir.path().join(LEGACY_TELEGRAM_FILE_NAME), serde_json::to_string(&telegram_seed).unwrap())
            .expect("seed legacy telegram file");

        let mut channel_seed = BTreeMap::new();
        channel_seed.insert("agent-a:email:password".to_owned(), "legacy-secret".to_owned());
        std::fs::write(dir.path().join(LEGACY_CHANNEL_FILE_NAME), serde_json::to_string(&channel_seed).unwrap())
            .expect("seed legacy channel file");

        let vault = SecretVault::open().expect("open");

        let got_mcp = vault.get_mcp("server_a").expect("get mcp").expect("mcp record folded into the vault");
        assert_eq!(got_mcp.access_token, mcp_record.access_token);
        assert_eq!(
            vault.get_telegram("agent-a").expect("get telegram"),
            Some("legacy-bot-token".to_owned()),
            "telegram record must be folded into the vault"
        );
        assert_eq!(
            vault.get_channel("agent-a", "email", "password").expect("get channel"),
            Some("legacy-secret".to_owned()),
            "channel record must be folded into the vault"
        );

        assert!(!dir.path().join(LEGACY_MCP_FILE_NAME).exists(), "legacy mcp file must be deleted after folding");
        assert!(
            !dir.path().join(LEGACY_TELEGRAM_FILE_NAME).exists(),
            "legacy telegram file must be deleted after folding"
        );
        assert!(
            !dir.path().join(LEGACY_CHANNEL_FILE_NAME).exists(),
            "legacy channel file must be deleted after folding"
        );
    }

    #[test]
    fn migration_with_nothing_legacy_present_is_a_clean_no_op() {
        let _lock = lock_env();
        MIGRATION_DONE.store(false, Ordering::SeqCst);
        let (_dir, _dd, _fb) = set_up();

        // No legacy files seeded — open must succeed and the vault starts empty.
        let vault = SecretVault::open().expect("open");
        assert!(vault.get_mcp("server_a").expect("get").is_none());
    }

    #[test]
    fn migration_guard_runs_at_most_once_per_process() {
        let _lock = lock_env();
        MIGRATION_DONE.store(false, Ordering::SeqCst);
        let (dir, _dd, _fb) = set_up();

        let mut mcp_seed = BTreeMap::new();
        mcp_seed.insert("server_a".to_owned(), sample_mcp_record());
        std::fs::write(dir.path().join(LEGACY_MCP_FILE_NAME), serde_json::to_string(&mcp_seed).unwrap())
            .expect("seed legacy mcp file");

        let vault = SecretVault::open().expect("first open");
        assert!(vault.get_mcp("server_a").expect("get").is_some());
        assert!(!dir.path().join(LEGACY_MCP_FILE_NAME).exists());

        // A legacy file "reappearing" after the guard has already fired this
        // process (e.g. a stale backup restore) must not be re-folded — the
        // guard is per-process, not per-call, so a second `open()` leaves it
        // alone entirely.
        let mut resurrected = BTreeMap::new();
        resurrected.insert("server_b".to_owned(), sample_mcp_record());
        std::fs::write(dir.path().join(LEGACY_MCP_FILE_NAME), serde_json::to_string(&resurrected).unwrap())
            .expect("write resurrected legacy file");

        let vault2 = SecretVault::open().expect("second open");
        assert!(vault2.get_mcp("server_b").expect("get").is_none(), "second open must not re-run migration");
        assert!(
            dir.path().join(LEGACY_MCP_FILE_NAME).exists(),
            "second open must not touch the resurrected legacy file"
        );
    }

    // --- File permissions ---

    #[cfg(unix)]
    #[test]
    fn file_is_written_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = lock_env();
        let (dir, _dd, _fb) = set_up();

        let vault = SecretVault::open().expect("open");
        vault.set_provider("anthropic", "sk-test-key").expect("set");

        let path = dir.path().join(FILE_STORE_NAME);
        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret_vault.json should be owner-read/write only, got {mode:o}");
    }

    // --- Force-file env vars ---

    #[test]
    fn unified_force_file_env_var_selects_file_backend() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set(FILE_FALLBACK_ENV_VAR, "1");

        let vault = SecretVault::open().expect("open");
        assert!(matches!(vault.backend, Backend::File(_)));
    }

    #[test]
    fn legacy_mcp_force_file_env_var_is_honored_as_alias() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_MCP_STORE_FILE_FALLBACK", "1");

        let vault = SecretVault::open().expect("open");
        assert!(matches!(vault.backend, Backend::File(_)));
    }

    #[test]
    fn legacy_telegram_force_file_env_var_is_honored_as_alias() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_TELEGRAM_STORE_FILE_FALLBACK", "1");

        let vault = SecretVault::open().expect("open");
        assert!(matches!(vault.backend, Backend::File(_)));
    }

    #[test]
    fn legacy_channel_force_file_env_var_is_honored_as_alias() {
        let _lock = lock_env();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _fb = EnvGuard::set("LAUNCHPAD_CHANNEL_SECRET_STORE_FILE_FALLBACK", "1");

        let vault = SecretVault::open().expect("open");
        assert!(matches!(vault.backend, Backend::File(_)));
    }

    // --- Keychain-prompt guards: kill switch, CI, test-harness detection ---
    //
    // These are the guards this module added to stop unattended processes
    // from ever blocking on a macOS keychain permission prompt (see the
    // module-level doc section "No unattended process may ever block on a
    // keychain prompt"). The two `keychain_forbidden` tests below are the
    // load-bearing ones: they prove the auto-detection alone — with every
    // other guard variable explicitly cleared — is enough to keep a cargo
    // test binary off the real keychain even if a test author forgets to
    // set `LAUNCHPAD_SECRET_VAULT_FILE_FALLBACK` by hand, which is exactly
    // the gap that let a keychain prompt block a background agent run.

    /// Clears every keychain-avoidance env var this module recognizes, so a
    /// test can prove a guard's effect (or, for `keychain_forbidden`, the
    /// *absence* of any explicit guard) without depending on what the
    /// ambient shell happens to have set.
    fn clear_all_keychain_guard_env_vars() -> Vec<EnvGuard> {
        let mut guards = vec![EnvGuard::unset(FILE_FALLBACK_ENV_VAR), EnvGuard::unset(NO_KEYCHAIN_ENV_VAR), EnvGuard::unset("CI")];
        guards.extend(LEGACY_FILE_FALLBACK_ENV_VARS.iter().map(|v| EnvGuard::unset(v)));
        guards
    }

    /// Proves `running_from_test_harness` actually fires for the real
    /// executable `cargo test` is running right now — not just for a
    /// synthetic path built by hand. This is the closest a unit test can
    /// come to "empirically prove it fires from another crate's test
    /// binary": the function makes its decision purely from the *shape* of
    /// `std::env::current_exe()` (`target/<profile>/deps/<name>-<hash>`),
    /// with nothing crate-specific in the check, so a positive result here
    /// generalizes to every other crate's cargo-built test/bench binary,
    /// including `ao-engine`'s — the crate whose test binary is what
    /// actually triggered the keychain prompt this guard exists to stop.
    #[test]
    fn running_from_test_harness_detects_the_real_running_cargo_test_binary() {
        assert!(
            running_from_test_harness(),
            "expected this cargo-test process's own executable ({:?}) to match the \
             target/<profile>/deps/<name>-<hash> shape cargo always uses for test binaries",
            std::env::current_exe(),
        );
    }

    /// The load-bearing regression test: a cargo test binary must be kept
    /// off the real keychain automatically, with no test-author action
    /// required. Every other force-file/kill-switch env var is explicitly
    /// cleared first, so the only thing left that could make this `true` is
    /// the test-harness auto-detection itself.
    #[test]
    fn keychain_forbidden_is_true_inside_a_test_binary_even_with_every_other_guard_cleared() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        assert!(
            keychain_forbidden(),
            "a cargo test binary must never be allowed to reach the real OS keychain, \
             even when no force-file/kill-switch env var is set"
        );
    }

    /// `LAUNCHPAD_TEST_KEYCHAIN` predates this guard (see
    /// `channel_secret_store::tests::keychain_round_trip` and its siblings)
    /// and must keep working: a developer who deliberately opts a specific
    /// `#[ignore]`d test into exercising the real keychain must not have
    /// the new cross-crate test-harness auto-detection silently redirect
    /// them to the file backend instead.
    #[test]
    fn real_keychain_opt_in_overrides_test_harness_auto_detection() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let _opt_in = EnvGuard::set("LAUNCHPAD_TEST_KEYCHAIN", "1");
        assert!(
            !keychain_forbidden(),
            "LAUNCHPAD_TEST_KEYCHAIN=1 must let a test binary reach the real keychain despite \
             test-harness auto-detection"
        );
    }

    #[test]
    fn no_keychain_env_var_selects_file_backend_even_without_file_fallback_set() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _nk = EnvGuard::set(NO_KEYCHAIN_ENV_VAR, "1");

        let vault = SecretVault::open().expect("open");
        assert!(matches!(vault.backend, Backend::File(_)));
    }

    #[test]
    fn ci_env_var_selects_file_backend_even_without_file_fallback_set() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let dir = tempfile::tempdir().expect("tempdir");
        let _dd = EnvGuard::set(DATA_DIR_ENV_VAR, dir.path().to_str().unwrap());
        let _ci = EnvGuard::set("CI", "true");

        let vault = SecretVault::open().expect("open");
        assert!(matches!(vault.backend, Backend::File(_)));
    }

    // --- should_suppress_keychain_prompts precedence ---
    //
    // Every test here uses `clear_all_keychain_guard_env_vars` plus
    // `LAUNCHPAD_TEST_KEYCHAIN` (see `real_keychain_opt_in_overrides_test_harness_auto_detection`
    // above) to force `keychain_forbidden()` to `false` when a test needs
    // to reach the final "allow the prompt" branch — otherwise every test
    // in this crate would trivially observe `keychain_forbidden() == true`
    // just by virtue of running inside a cargo test binary, and the
    // precedence table below would never exercise past step 2.
    //
    // Each also unsets `ALLOW_KEYCHAIN_PROMPT_ENV_VAR` unless it is the
    // thing under test: it is an escape hatch a developer debugging this
    // very code path is likely to have exported in their shell, and leaving
    // it ambient would silently short-circuit step 1 and make the
    // suppression assertions pass for the wrong reason.

    #[test]
    fn allow_prompt_env_var_wins_even_when_keychain_forbidden_is_true() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let _nk = EnvGuard::set(NO_KEYCHAIN_ENV_VAR, "1");
        let _allow = EnvGuard::set(ALLOW_KEYCHAIN_PROMPT_ENV_VAR, "1");

        assert!(keychain_forbidden(), "test setup: expected the kill switch to forbid the keychain");
        assert!(
            !should_suppress_keychain_prompts(),
            "the escape hatch must win over keychain_forbidden()"
        );
    }

    #[test]
    fn keychain_forbidden_suppresses() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let _no_allow = EnvGuard::unset(ALLOW_KEYCHAIN_PROMPT_ENV_VAR);
        let _nk = EnvGuard::set(NO_KEYCHAIN_ENV_VAR, "1");

        assert!(keychain_forbidden(), "test setup: expected the kill switch to forbid the keychain");
        assert!(should_suppress_keychain_prompts(), "a keychain-forbidden process must suppress prompts");
    }

    /// The regression test for the narrowing this gate underwent: an earlier
    /// revision suppressed for *any* `cfg!(debug_assertions)` build, which
    /// broke the interactive dev server — every keychain read failed with
    /// `errSecAuthFailed` and the developer never saw the password popup
    /// they were meant to answer.
    ///
    /// This test binary is itself a debug build, so `cfg!(debug_assertions)`
    /// is `true` here by construction: with the keychain not forbidden and
    /// no escape-hatch env var set, a `false` result is only possible if the
    /// build profile genuinely no longer participates in the decision.
    #[test]
    fn debug_build_does_not_suppress_when_keychain_is_not_forbidden() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let _no_allow = EnvGuard::unset(ALLOW_KEYCHAIN_PROMPT_ENV_VAR);
        let _opt_in = EnvGuard::set("LAUNCHPAD_TEST_KEYCHAIN", "1");

        assert!(cfg!(debug_assertions), "test setup: expected cargo test to build this binary in debug profile");
        assert!(!keychain_forbidden(), "test setup: expected the real-keychain opt-in to clear keychain_forbidden()");
        assert!(
            !should_suppress_keychain_prompts(),
            "a debug build must let a legitimate prompt draw once the keychain is in play — \
             the build profile is not a suppression signal"
        );
    }

    // --- propagate_keychain_forbidden (inheritance by construction) ---
    //
    // These exercise `propagate_keychain_forbidden_once` — the decision-and-
    // write logic behind `propagate_keychain_forbidden` — directly, since
    // the public function's `OnceLock` only ever fires once for the life of
    // a process and so can't be meaningfully re-exercised across more than
    // one env-var combination within a single test binary. The public
    // function's "fires at most once, ever" contract is proven by the
    // separate spawned-child reachability test in `ao-engine-tools-cli`
    // (see `keychain_propagation_reaches_spawned_children.rs`), which runs
    // in a fresh process where that distinction doesn't matter.

    #[test]
    fn propagate_once_sets_the_kill_switch_when_keychain_is_forbidden() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        // Every test binary satisfies `running_from_test_harness()` on its
        // own, so `keychain_forbidden()` is already `true` here with every
        // other guard cleared — no extra env var needed to force it.
        assert!(keychain_forbidden(), "test setup: expected this test binary to already be keychain-forbidden");

        propagate_keychain_forbidden_once();

        assert_eq!(
            std::env::var(NO_KEYCHAIN_ENV_VAR).as_deref(),
            Ok("1"),
            "propagation must reify the forbidden decision into this process's own environment"
        );
    }

    #[test]
    fn propagate_once_does_nothing_when_keychain_is_not_forbidden() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let _opt_in = EnvGuard::set("LAUNCHPAD_TEST_KEYCHAIN", "1");
        assert!(!keychain_forbidden(), "test setup: expected the real-keychain opt-in to clear keychain_forbidden()");

        propagate_keychain_forbidden_once();

        assert!(
            std::env::var(NO_KEYCHAIN_ENV_VAR).is_err(),
            "propagation must not invent a forbidden decision that keychain_forbidden() itself didn't reach"
        );
    }

    /// The required non-regression: the escape hatch must keep winning even
    /// for propagation, not just for `should_suppress_keychain_prompts`. An
    /// operator who explicitly asked to see a real keychain prompt from
    /// this process is debugging real keychain behavior — forcing every
    /// child it spawns off the keychain too would fight that intent.
    #[test]
    fn propagate_once_does_not_force_children_off_the_keychain_when_allow_prompt_escape_hatch_is_set() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        let _fb = EnvGuard::set(FILE_FALLBACK_ENV_VAR, "1");
        let _allow = EnvGuard::set(ALLOW_KEYCHAIN_PROMPT_ENV_VAR, "1");
        assert!(keychain_forbidden(), "test setup: expected the forced file-fallback to forbid the keychain");

        propagate_keychain_forbidden_once();

        assert!(
            std::env::var(NO_KEYCHAIN_ENV_VAR).is_err(),
            "the allow-prompt escape hatch must stop propagation from forcing children off the keychain too"
        );
    }

    #[test]
    fn propagate_once_is_idempotent_and_does_not_clobber_an_already_set_value() {
        let _lock = lock_env();
        let _guards = clear_all_keychain_guard_env_vars();
        assert!(keychain_forbidden(), "test setup: expected this test binary to already be keychain-forbidden");

        propagate_keychain_forbidden_once();
        propagate_keychain_forbidden_once();

        assert_eq!(std::env::var(NO_KEYCHAIN_ENV_VAR).as_deref(), Ok("1"));
    }

    // --- Backend introspection ---

    #[test]
    fn file_backed_vault_reports_not_keychain_backed() {
        let _lock = lock_env();
        let (_dir, _dd, _fb) = set_up();
        let vault = SecretVault::open().expect("open");
        assert!(!vault.is_keychain_backed());
    }

    // --- Test constructor never touches the real keychain ---

    #[test]
    fn new_with_file_fallback_always_selects_file_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = SecretVault::new_with_file_fallback(dir.path().to_path_buf());
        assert!(matches!(vault.backend, Backend::File(_)));

        vault.set_provider("anthropic", "sk-test-key").expect("set");
        assert_eq!(vault.get_provider("anthropic").expect("get"), Some("sk-test-key".to_owned()));
        assert!(dir.path().join(FILE_STORE_NAME).exists());
    }
}
