//! IO tool implementations for the native engine.
//!
//! This crate provides the full IO tool catalog used by the engine
//! runner: [`Read`], [`Glob`], [`Grep`], [`Edit`], [`Write`], [`NotebookEdit`],
//! and [`BashTool`]. They are dispatched through the locked `IoTool` trait +
//! `Registry` defined in `ao-engine-tools-core` — there is no in-crate
//! runtime; the engine invokes each tool by name through the registry.
//!
//! # Tools
//!
//! | Tool           | Input schema                                                                                                                                     |
//! |----------------|--------------------------------------------------------------------------------------------------------------------------------------------------|
//! | `Read`         | `file_path` (required, absolute), `offset` (default `0`), `limit` (default `2000`)                                                              |
//! | `Glob`         | `pattern` (required), `path` (optional, absolute, defaults to cwd)                                                                               |
//! | `Grep`         | `pattern` (required), `path`, `glob`, `type`, `output_mode`, `-A`/`-B`/`-C`, `-i`/`-n`, `multiline`, `head_limit`, `offset`                    |
//! | `Edit`         | `file_path` (required, absolute), `old_string` (required), `new_string` (required), `replace_all` (default `false`)                             |
//! | `Write`        | `file_path` (required, absolute), `content` (required)                                                                                           |
//! | `NotebookEdit` | `notebook_path` (required), `cell_type`, `source`, `cell_index`, `edit_mode`                                                                    |
//! | `Bash`         | `command` (required), `timeout` (ms, optional, default 120000, max 600000), `description` (optional), `run_in_background` (bool, default false) |
//!
//! Each tool's full description is returned by
//! [`IoTool::description`](ao_engine_tools_core::IoTool::description) — see
//! the `prompt` submodule of each tool for the exact constant.
//!
//! # Per-tool folder layout
//!
//! Every tool lives under `src/<tool>/` with a fixed three-file layout
//! The caller pattern:
//!
//! - `mod.rs` — the [`IoTool`](ao_engine_tools_core::IoTool) implementation.
//! - `prompt.rs` — `pub const DESCRIPTION: &str = "..."` with the prompt text
//!   versioned alongside the implementation.
//! - `tests.rs` — every unit test, declared from `mod.rs` as
//!   `#[cfg(test)] mod tests;`. NO inline `#[cfg(test)] mod tests { ... }`
//!   blocks live in `mod.rs`.
//!
//! Phases 2+ layer permission predicates and UI metadata onto this layout
//! without restructuring the tree.
//!
//! # Grep is in-process, not a shell-out
//!
//! [`Grep`] is built on the ripgrep library crates (`grep`, `grep-regex`,
//! `grep-searcher`, `grep-printer`, `ignore`) — these crates ARE ripgrep,
//! exposed as Rust APIs from the `BurntSushi/ripgrep` repository. We link
//! them directly rather than shelling out to a vendored binary. Linking
//! avoids the fork/exec + IPC cost on every search and removes per-platform
//! binary distribution from the engine.
//!
//! # Bash
//!
//! [`BashTool`] executes shell commands via a fresh `/bin/bash -c` subprocess
//! per call — there is no persistent session between tool invocations.
//!
//! **Foreground execution path** — `execute::run` spawns the child, captures
//! stdout and stderr into separate buffers concurrently, enforces a
//! configurable timeout (default 120 s, max 600 s) with SIGTERM → 5-second
//! grace → SIGKILL escalation, and propagates the runner's cancellation token
//! through the same escalation path. Output is **middle-truncated** to a
//! combined 30 KB budget so large command output does not overflow the model's
//! context window.
//!
//! **Leading-`cd` lifting** — a pre-parser strips a leading `cd <path> &&`
//! (or `; `) from the command and resolves the path against `RunnerContext::cwd`
//! before spawning. Only the remainder of the command runs in the subprocess.
//!
//! **Env-scrub denylist** — the subprocess inherits the runner's environment
//! with `AO_*`, `LAUNCHPAD_*`, and `CLAUDE_*` keys removed. A `BASH_ENV`
//! pointing at a `set -o pipefail` snippet is injected so pipeline failures
//! surface as non-zero exit codes.
//!
//! **~200-LOC command classifier** — `command_classifier::classify` returns a
//! `Classification` enum (`ReadOnly | Destructive | NetworkTouching |
//! GitMutating | Unclassified`) used to enrich permission-denial prompts. This
//! is a **UX-enrichment layer only, not a security boundary**.
//!
//! **Background-mode skeleton** — when `run_in_background: true`, the tool
//! spawns the subprocess and registers it in
//! `ao_engine_tools_core::background_processes::BackgroundProcessRegistry`,
//! returning a `process_id` immediately. The subprocess then runs
//! independently and is owned by the registry.
//!
//! **Deferrals** (out of scope for this bucket, tracked for follow-up):
//! - Sandbox / policy isolation
//! - Persistent shell session across calls
//! - Full reference-policy classifier (~5.9 KLOC)
//! - Background-mode polling and stopping (`BashStatus` / `BashKill`)
//!
//! # Permissions and hooks
//!
//! No tool in this crate carries its own access predicate: the six
//! filesystem tools take the `IoTool::check_permissions` default of
//! `PermissionDecision::Allow`, and only the `Bash` family overrides it.
//! The gate itself lives in `ao-engine-tools-runner`'s query loop, which
//! calls `check_permissions` on every dispatch and feeds the result through
//! `evaluate_permission` together with the session permission mode and the
//! configured policy — so a tool returning `Allow` can still be denied, or
//! routed to the `UserPromptBridge` to ask, before `invoke` runs.
//! PreTool/PostTool hooks run around that same dispatch point.
//!
//! # Registering with a `Registry`
//!
//! ```no_run
//! use ao_engine_tools_core::Registry;
//! use ao_engine_tools_io::register_all;
//!
//! let mut registry = Registry::new();
//! register_all(&mut registry);
//! assert!(registry.lookup_io("Read").is_some());
//! assert!(registry.lookup_io("Glob").is_some());
//! assert!(registry.lookup_io("Grep").is_some());
//! assert!(registry.lookup_io("Edit").is_some());
//! assert!(registry.lookup_io("Write").is_some());
//! assert!(registry.lookup_io("NotebookEdit").is_some());
//! assert!(registry.lookup_io("Bash").is_some());
//! assert!(registry.lookup_io("BashStatus").is_some());
//! assert!(registry.lookup_io("BashKill").is_some());
//! ```

use std::sync::Arc;

use ao_engine_tools_core::Registry;

pub mod bash;
pub mod bash_kill;
pub mod bash_status;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod notebook_edit;
pub mod path;
pub mod read;
pub mod write;

pub use bash::BashTool;
pub use bash_kill::BashKill;
pub use bash_status::BashStatus;
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use notebook_edit::NotebookEdit;
pub use path::expand_path;
pub use read::Read;
pub use write::Write;

/// Register every IO tool ([`Read`], [`Glob`], [`Grep`], [`Edit`],
/// [`Write`], [`NotebookEdit`], [`BashTool`], [`BashStatus`], [`BashKill`])
/// into the supplied [`Registry`] via `register_io`. External callers (e.g.
/// the engine session bootstrap) use this to install the entire catalog with
/// one call instead of knowing each tool's struct name.
pub fn register_all(registry: &mut Registry) {
    registry.register_io(Arc::new(Read));
    registry.register_io(Arc::new(Glob));
    registry.register_io(Arc::new(Grep));
    registry.register_io(Arc::new(Edit));
    registry.register_io(Arc::new(Write));
    notebook_edit::register_notebook_edit(registry);
    bash::register_bash(registry);
    bash_status::register_bash_status(registry);
    bash_kill::register_bash_kill(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_engine_tools_core::Registry;

    #[test]
    fn register_all_installs_nine_io_tools() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert_eq!(r.len(), 9);
    }

    #[test]
    fn register_all_tool_names_present() {
        let mut r = Registry::new();
        register_all(&mut r);
        assert!(r.lookup_io("Read").is_some());
        assert!(r.lookup_io("Glob").is_some());
        assert!(r.lookup_io("Grep").is_some());
        assert!(r.lookup_io("Edit").is_some());
        assert!(r.lookup_io("Write").is_some());
        assert!(r.lookup_io("NotebookEdit").is_some());
        assert!(r.lookup_io("Bash").is_some());
        assert!(r.lookup_io("BashStatus").is_some());
        assert!(r.lookup_io("BashKill").is_some());
    }

    #[test]
    fn register_all_schema_export_contains_all_tools() {
        let mut r = Registry::new();
        register_all(&mut r);
        let export = r.schema_export();
        let entries = export.as_array().expect("schema_export returns an array");
        let names: Vec<&str> = entries
            .iter()
            .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
            .collect();
        for tool in [
            "Read",
            "Glob",
            "Grep",
            "Edit",
            "Write",
            "NotebookEdit",
            "Bash",
            "BashStatus",
            "BashKill",
        ] {
            assert!(
                names.contains(&tool),
                "missing tool in schema_export: {tool}"
            );
            let entry = entries
                .iter()
                .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(tool))
                .unwrap();
            let schema = entry.get("input_schema").expect("input_schema key present");
            assert!(
                !schema.is_null(),
                "input_schema for {tool} must be non-null"
            );
        }
    }
}
