use std::path::{Path, PathBuf};
use std::sync::Arc;

use ao_persistence::memory::MemoryStore;
use ao_persistence::{hash_project_key, resolve_project_key};
use ao_protocol::error::AoError;
use ao_protocol::memory::MemoryScope;
use ao_protocol::thread::default_thread_id;

// --- Per-scope entry caps ---
pub const GLOBAL_SOFT_CAP: usize = 25;
pub const GLOBAL_HARD_CAP: usize = 40;
pub const AGENT_SOFT_CAP: usize = 60;
pub const AGENT_HARD_CAP: usize = 100;
pub const PROJECT_SOFT_CAP: usize = 80;
pub const PROJECT_HARD_CAP: usize = 150;
pub const ENTRY_CHAR_SOFT: usize = 2000;
pub const ENTRY_CHAR_HARD: usize = 8000;

// Thread scope is a small, ephemeral working-memory tier (dies with the
// thread, no promotion path of its own) — its caps stay well below every
// durable scope's, on both entry count and entry length. These are
// deliberately separate constants, not a scaled-down reuse of the durable
// caps above, so tightening a durable scope's cap can never accidentally
// tighten (or loosen) this one.
pub const THREAD_SOFT_CAP: usize = 15;
pub const THREAD_HARD_CAP: usize = 25;
pub const THREAD_ENTRY_CHAR_SOFT: usize = 500;
pub const THREAD_ENTRY_CHAR_HARD: usize = 2000;

/// Resolved storage context for a memory operation.
#[derive(Debug, Clone)]
pub enum ScopeContext {
    Agent { agent_id: String },
    Project { hash: String, canonical_key: String },
    Global,
    /// The `agent×project` cell reserved by [`MemoryScope::AgentProject`]:
    /// this agent's memory about this specific repo. `key` is the resolvable
    /// identity of the cell — see [`agent_project_key`] for its derivation.
    ///
    /// Resolvable today via [`resolve_scope_context`]; no `MemoryStore`
    /// method reads or writes it yet (that lands with the reflection pass —
    /// see workstream F in the memory/skills plan).
    AgentProject {
        agent_id: String,
        project_hash: String,
        key: String,
    },
    /// The ephemeral per-thread working-memory tier keyed by
    /// [`MemoryScope::Thread`]. `thread_id` is the current runner context's
    /// thread id, or the agent's default thread id (see
    /// [`default_thread_id`]) when the run has no explicit thread — see
    /// [`resolve_scope_context`] for details.
    Thread { thread_id: String },
}

/// Resolve the storage context for a given scope.
///
/// For `Project` scope this spawns `git rev-parse --show-toplevel` (or falls
/// back to canonicalize) and hashes the result. For other scopes it is O(1).
///
/// Resolution priority for `Project` scope:
/// 1. `explicit_working_dir` — model-supplied override (wins over everything)
/// 2. `parent_cwd` — parent agent's cwd snapshot at delegation time
/// 3. `ctx_cwd` — current session's cwd (top-level agent default)
///
/// `explicit_working_dir` and `parent_cwd` are both `None` for top-level
/// agents, so resolution falls straight through to `ctx_cwd` (priority 3
/// above).
///
/// `thread_id` only matters for `Thread` scope, where it is the runner
/// context's current thread id. When `Thread` scope is requested and
/// `thread_id` is `None` (or blank), resolution falls back to the agent's
/// default thread id ([`default_thread_id`]) — mirroring `ListThreads`'s
/// treatment of the implicit main-conversation thread as a real,
/// addressable thread. This still resolves to `Thread` scope, keyed to that
/// default thread id; it never falls through to agent, project, or global
/// scope.
pub async fn resolve_scope_context(
    scope: &MemoryScope,
    agent_id: &str,
    explicit_working_dir: Option<&Path>,
    parent_cwd: Option<&Path>,
    ctx_cwd: &Path,
    thread_id: Option<&str>,
) -> Result<ScopeContext, AoError> {
    match scope {
        MemoryScope::Agent => Ok(ScopeContext::Agent {
            agent_id: agent_id.to_string(),
        }),
        MemoryScope::Global => Ok(ScopeContext::Global),
        MemoryScope::Project => {
            let cwd = explicit_working_dir
                .or(parent_cwd)
                .unwrap_or(ctx_cwd);
            let canonical_key = resolve_project_key(cwd).await?;
            let hash = hash_project_key(&canonical_key);
            Ok(ScopeContext::Project {
                hash,
                canonical_key,
            })
        }
        MemoryScope::AgentProject => {
            let cwd = explicit_working_dir
                .or(parent_cwd)
                .unwrap_or(ctx_cwd);
            let canonical_key = resolve_project_key(cwd).await?;
            let project_hash = hash_project_key(&canonical_key);
            let key = agent_project_key(agent_id, &project_hash);
            Ok(ScopeContext::AgentProject {
                agent_id: agent_id.to_string(),
                project_hash,
                key,
            })
        }
        MemoryScope::Thread => {
            let id = match thread_id {
                Some(id) if !id.trim().is_empty() => id.to_string(),
                _ => default_thread_id(agent_id),
            };
            Ok(ScopeContext::Thread { thread_id: id })
        }
    }
}

/// Derive the resolvable key for the `agent×project` cell by combining an
/// agent id with a project hash.
///
/// The two identifiers are joined with an ASCII unit-separator byte (`\x1f`,
/// a character that cannot appear in either an agent id or a hex project
/// hash) before hashing, so no combination of a shorter agent id and a
/// longer project hash can collide with a different split of the same
/// concatenated bytes. The result is stable for a given `(agent_id,
/// project_hash)` pair and distinct across every other pair: the same agent
/// in two repos gets two keys, and two agents in the same repo get two keys.
pub fn agent_project_key(agent_id: &str, project_hash: &str) -> String {
    hash_project_key(&format!("{agent_id}\u{1f}{project_hash}"))
}

/// Resolve a model-supplied `working_dir` against the runner cwd.
///
/// Memory tools accept an optional `working_dir` so an agent that has navigated
/// or `cd`'d outside the runner's launch directory can pin project-scope reads
/// and writes to the repo it's actually editing in. The runner's `ctx.cwd`
/// only moves via EnterWorktree/ExitWorktree, not via `Bash cd`, so without
/// this override every project-scope call would resolve relative to the
/// session's original launch directory.
///
/// - Empty input falls back to `cwd`.
/// - `~` and `~/<rest>` expand against `$HOME` (or `%USERPROFILE%` on Windows).
/// - Absolute paths pass through unchanged.
/// - Relative paths are joined onto `cwd`.
///
/// If `HOME` is unset, tilde forms gracefully fall back to a literal join so
/// the helper never panics.
pub fn resolve_working_dir(input: Option<&str>, cwd: &Path) -> PathBuf {
    let raw = match input {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => return cwd.to_path_buf(),
    };

    let home = home_dir();
    if raw == "~" {
        return home.unwrap_or_else(|| cwd.join("~"));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return match home {
            Some(h) => h.join(rest),
            None => cwd.join(raw),
        };
    }

    let candidate = PathBuf::from(raw);
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(raw)
    }
}

fn home_dir() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(h) = std::env::var("USERPROFILE") {
            if !h.is_empty() {
                return Some(PathBuf::from(h));
            }
        }
    }
    None
}

/// Check a content string against the character caps.
///
/// Returns `Ok(None)` if within soft cap, `Ok(Some(warning))` if between soft
/// and hard, `Err` if over hard cap.
pub fn check_entry_caps(content: &str) -> Result<Option<String>, AoError> {
    let len = content.chars().count();
    if len > ENTRY_CHAR_HARD {
        return Err(AoError::Internal(format!(
            "Entry is too long ({len} chars). Maximum is {ENTRY_CHAR_HARD} chars."
        )));
    }
    if len > ENTRY_CHAR_SOFT {
        return Ok(Some(format!(
            "⚠ Entry is long ({len} chars). Consider summarizing to keep memories concise."
        )));
    }
    Ok(None)
}

/// Check whether a scope is near or over its entry cap by counting live entries.
///
/// Returns `Ok(None)` if within soft cap, `Ok(Some(warning))` if within
/// soft-hard range, `Err` if over hard cap.
pub async fn check_scope_caps(
    store: &Arc<MemoryStore>,
    scope_ctx: &ScopeContext,
    soft: usize,
    hard: usize,
) -> Result<Option<String>, AoError> {
    let count = match scope_ctx {
        ScopeContext::Agent { agent_id } => store.list(agent_id).await?.len(),
        ScopeContext::Global => store.list_global().await?.len(),
        ScopeContext::Project { hash, .. } => store.list_project(hash).await?.len(),
        ScopeContext::Thread { thread_id } => store.list_thread(thread_id).await?.len(),
        // Reserved cell — no `MemoryStore` method backs it yet, so there is
        // nothing on disk to count. No caller resolves this variant today.
        ScopeContext::AgentProject { .. } => 0,
    };

    if count >= hard {
        return Err(AoError::Internal(format!(
            "Memory scope is full ({count}/{hard} entries). Use MemoryList + MemoryDelete to free space."
        )));
    }
    if count >= soft {
        return Ok(Some(format!(
            "⚠ Memory scope at {count}/{soft}. Consider deleting stale or contradicted entries before adding more."
        )));
    }
    Ok(None)
}
