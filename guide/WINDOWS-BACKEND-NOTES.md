# Windows backend compile findings — 2026-10-06

Repository: /Users/ebongandrew/dev/launchpad_studio-tools
Task: 2ae95796-1d69-4fcf-ba1d-c8196cc4c4e6

## Changes

- `crates/ao-process/src/lib.rs`, `kill_tree.rs`: expose process-tree termination on Windows; use `taskkill.exe /PID <pid> /T /F` so cancellation/timeouts include descendants. Unix SIGTERM/grace/SIGKILL behavior remains unchanged. Windows termination is immediate and logs command failures.
- `crates/ao-process/src/shell.rs`: shared Bash resolver. On Windows, honor `LAUNCHPAD_GIT_BASH`, search Git installation directories under ProgramFiles/ProgramFiles(x86)/LOCALAPPDATA and PATH (including adjacent bin), reject System32 WSL launcher, and return an actionable Git for Windows installation error. Native Unix execution retains SHELL-bash preference and /bin/bash fallback. Helpers normalize Windows paths for Bash environment/source statements and safely quote apostrophes in source paths.
- `crates/ao-process/src/default_supervisor.rs`: resolve Windows extensionless CLI names using the sibling task's `ao_process::executable::resolve`, including npm .cmd shims and the input's PATH override. Arguments remain passed separately to Rust Command, preserving Rust batch argument encoding. Unix command construction remains unchanged.
- `crates/ao-engine-tools-io/src/bash/execute.rs`: resolve Bash with errors propagated on all execution paths; use tree termination on Windows; normalize BASH_ENV and CWD_CAPTURE_FILE paths; capture Windows cwd using Git Bash `pwd -W`, while Unix retains `pwd -P`; quote shell snapshot source paths.
- `crates/ao-engine-tools-io/src/bash/shell_snapshot.rs`: use Git Bash detection on Windows; retain PATH-based `bash` snapshot invocation on Unix; use Bash printf %q for PATH exports so paths containing spaces source correctly. Existing libc atexit calls are supported on the Windows target and need no Unix-only gate. Unix process_group and ExitStatusExt calls were already cfg-gated.
- `crates/ao-engine-tools-runner/src/hooks/mod.rs`, `crates/ao-engine/src/workflow_runner.rs`: use detected Git Bash on Windows. Missing Bash produces a hook warning (retaining hook fail-open behavior) or a workflow Process error. Unix retains its existing PATH-based bash invocation.
- `crates/ao-engine-tools-io/Cargo.toml`, `crates/ao-engine-tools-runner/Cargo.toml`, `Cargo.lock`: add shared ao-process dependency.

## Validation

- Inspected ancestor and repository AGENTS.md locations and repository file search; none found. Initial git status was clean. Preserved sibling edits that appeared during execution (executable.rs, MCP client, Tauri CLI launch path).
- Installed Rust target `x86_64-pc-windows-gnu` successfully.
- PASS: `cargo check -p ao-process --target x86_64-pc-windows-gnu`.
- PASS: `cargo check -p ao-process --tests --target x86_64-pc-windows-gnu` (one existing unused-pid warning in Unix-gated test body). This checks the Windows supervisor, termination module, resolver, and npm shim test code; it does not execute Windows tests.
- PASS: macOS `cargo check -p ao-engine` (including native IO tools, hooks runner, Claude/Anthropic and OpenAI providers, and workflow backend).
- PASS: macOS `cargo test -p ao-process --lib`: 14 tests, including process cancellation, timeout, suspension/watchdog behavior and shell source quoting.
- PASS: macOS shell snapshot unit tests: 2 tests, including aliases/functions sourced through BASH_ENV; rechecked after PATH quoting change.
- PASS: native Bash smoke tests `bash_echo_hello`, `bash_cd_lifted_runs_in_target`, `bash_pipefail_visible`: 3 tests.
- PASS: `git diff --check`.

Used isolated CARGO_TARGET_DIR directories under /tmp to avoid a shared build lock. Detailed logs are copied next to workspace output.txt.

## Outstanding validation/build limits

- Full `cargo check -p ao-engine --target x86_64-pc-windows-gnu` was attempted and blocked before complete backend type checking in `ring 0.17.14` and bundled `libsqlite3-sys 0.37.0`: cc-rs could not find `x86_64-w64-mingw32-gcc`. Installing rust-std alone does not supply the C compiler, Windows headers or native libraries. No complete Windows backend/Tauri build success is claimed.
- Next: use a Windows runner with Rust MSVC, Visual Studio C++ build tools and Tauri prerequisites (or a properly provisioned cross-compiler) to perform the full build and create the unsigned installer.
- Windows runtime smoke tests remain required: Git Bash discovery (standard/custom paths and missing installation); native Bash timeout/cancellation killing descendants; cwd capture; shell snapshots with spaces; workflow run.sh; hooks; Claude/Codex npm shims with spaces and argument metacharacters. No Windows VM is available here.
- Windows preview shell features require Git for Windows. Set LAUNCHPAD_GIT_BASH to the full bin/bash.exe path for custom installations. The override is read by the parent before internal-prefix variables are removed from native Bash subprocess environments.
- No signing, publication, release, commit, push, or installer creation was performed in this backend task.

Rust command behavior reference: https://doc.rust-lang.org/std/process/struct.Command.html (extensions other than .exe must be explicit; argument encoding is handled by Command).
