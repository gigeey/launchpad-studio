# Developing Launchpad Studio

This guide covers building Launchpad Studio from source, running the test
suites, and the parallel-track workflow used to run two checkouts side by side.

For what the app *does*, see the [README](../README.md).

## Architecture at a glance

Launchpad Studio is a [Tauri 2](https://tauri.app/) desktop application.

- **`frontend/`** — the React + TypeScript UI, shipped inside the Tauri shell.
- **`crates/ao-*`** — the Rust backend workspace:
  - `ao-engine` — the orchestration engine: agent runners, the tasklist /
    workflow / project schedulers, reflection, and skill distillation.
  - `ao-server` — the HTTP server the frontend talks to.
  - `ao-persistence` — on-disk stores (agent profiles, threads, memories,
    assignments, preferences) rooted at the user data directory.
  - `ao-protocol` — the shared types (`AgentProfile`, `Thread`, workflow and
    assignment definitions, …).
  - `ao-mcp-bridge` — Model Context Protocol connector integration.
  - `ao-normalizer` — normalizes output from CLI-driven agents.
  - `ao-search-index` — SQLite FTS5 full-text index for memory / skill / session
    search.
  - `ao-engine-tools-*` — the agent tool implementations (filesystem, engine,
    provider clients, transports, …). Anthropic, OpenAI and OpenRouter are the
    three reachable from an agent profile; the Gemini crate is present but
    unwired — see [KNOWN-GAPS.md](../KNOWN-GAPS.md#the-gemini-provider-is-not-reachable).

## Prerequisites

- **Rust** — stable toolchain (install via [rustup](https://rustup.rs/)).
- **Node.js** — 20.19+, 22.13+, or 24+. This is the intersection of what the
  toolchain declares: `vite` needs `^20.19.0 || >=22.12.0` and `jsdom` needs
  `^20.19.0 || ^22.13.0 || >=24.0.0`. The same range is in
  `frontend/package.json`, and `frontend/.npmrc` sets `engine-strict=true` so
  `npm install` fails with a version error instead of warning and then
  breaking later inside a dependency.
- **A C compiler / build toolchain** (`cc`, plus `make`) — required at build
  time. The agent memory/skill search index (`ao-search-index`) uses SQLite's
  FTS5 full-text engine via `rusqlite` with the `bundled` feature, which
  **statically compiles SQLite from source** so FTS5 is always available
  regardless of the host's system SQLite. On macOS install the Xcode Command
  Line Tools (`xcode-select --install`); on Debian/Ubuntu `build-essential`; on
  Windows the MSVC build tools. No system SQLite install is needed.

> `rusqlite` is pinned to `=0.39.0` in the workspace manifest: `libsqlite3-sys`
> 0.38's build script relies on the unstable `cfg_select!` macro, which fails on
> the stable toolchain. Keep the pin until that is resolved upstream.

> `frontend/package.json` carries one `overrides` entry. `@emoji-mart/react`
> declares a peer range of `react@^16.8 || ^17 || ^18`, and its latest release
> (1.1.1) predates React 19, so `npm ci` fails outright with `ERESOLVE`. The
> override pins that one peer to the root `react` version. It does not change
> the installed tree — it records that the mismatch is accepted rather than
> unnoticed. Setting `legacy-peer-deps=true` would also silence it, but it would
> silence every future peer conflict in the project along with this one. The
> library is used only by `src/components/ui/EmojiPicker.tsx`; if that is ever
> replaced, delete the override with it.

## Build & run

```bash
cd frontend
npm install
npm run tauri dev
```

This builds the Rust backend, starts the dev server, and launches the desktop
shell with hot-reload on the frontend.

The first build compiles the whole dependency tree and needs roughly **9 GB of
free disk** for `target/`. It is by far the longest step; later builds are
incremental.

### Optional: stable dev signing on macOS

`cargo run` (and so `tauri dev`) links an ad-hoc signature that changes on
every rebuild, and macOS ties Keychain "Always Allow" grants to the signature —
so every rebuild re-triggers every Keychain prompt. To keep one identity across
rebuilds, export your Apple Development identity:

```bash
security find-identity -v -p codesigning          # list what you have
export LAUNCHPAD_DEV_SIGNING_IDENTITY="Apple Development: Your Name (TEAMID)"
```

`dev/dev-codesign-runner.sh`, wired in via
`.cargo/config.toml`, then re-signs the dev binary on each run. Leave the
variable unset and the script does nothing — no certificate is required to
build or run, only to silence the repeat prompts. It never affects
`cargo build`, so release signing is untouched.

## Tests

```bash
# Rust
cargo test --workspace --no-fail-fast

# Frontend
cd frontend
npx vitest run
```

**Use `--no-fail-fast`.** Without it cargo stops at the first test *binary* that
fails and prints a partial tally that reads exactly like a complete one.

**Use `cargo test`, not `cargo test --lib`.** Some MCP unit tests spawn a
fixture binary (`echo_mcp_server`) located on disk rather than through cargo, so
`--lib` neither builds it (≈51 spurious failures on a fresh clone) nor rebuilds
it after you edit `crates/ao-engine-tools-runner/tests/fixtures/echo_mcp_server.rs`
(a silent pass against the stale build). Rationale and the full caveat list are
in `crates/ao-engine-tools-runner/src/mcp/test_support.rs`.

**On macOS, four tests fail unless you disable keychain access.** They need
`LAUNCHPAD_STUDIO_NO_KEYCHAIN=1`, and they are not something you broke —
[KNOWN-GAPS.md](../KNOWN-GAPS.md) has the diagnosis, and
[CONTRIBUTING.md](../CONTRIBUTING.md#running-the-tests) has the full test-suite
walkthrough.

**No test can use a bare `sleep` of two seconds or more.** The Bash tool rejects
it synchronously, before any child is spawned (`detect_bare_sleep` in
`crates/ao-engine-tools-io/src/bash/mod.rs`), so a fixture built on `sleep` is
not slow — it never runs at all. Use `tail -f /dev/null` when a test needs a
live process, and assert elapsed-time bounds so the test cannot pass for the
wrong reason. [CONTRIBUTING.md](../CONTRIBUTING.md#running-the-tests) has the
worked example.

## Parallel-track development (worktree contract)

Two checkouts of this repo can run side by side without clobbering each other's
runtime state, build artifacts, or ports — useful when a long-running agent task
runs in one checkout while you keep working in another.

### One-time setup

```bash
git worktree add ../launchpad_studio-tools <branch>
```

### Environment contract

Every parallel checkout that runs the app **must** override these env vars; the
values below are the convention for a second worktree:

| Var | Purpose | Default | Second worktree |
|---|---|---|---|
| `LAUNCHPAD_STUDIO_DATA_DIR` | User data root (agents, transcripts, memories, preferences, assignments). Resolved by `ao_protocol::data_root::resolve_data_root` and consumed by `ao_persistence::DataRoot::resolve`. | `~/.launchpad_studio` | `~/.launchpad_studio-tools` |
| `AO_PORT` | `ao-server` HTTP listener (also read by the Tauri shell when it embeds the server). | `3001` | `3101` |
| `LAUNCHPAD_VITE_PORT` | Vite dev server port. Read by `frontend/vite.config.ts`. HMR uses `port + 1`. | `1420` | `1430` |
| `VITE_API_BASE_URL` | Frontend → backend base URL (must match `AO_PORT`). | `http://localhost:3001` | `http://localhost:3101` |

`CARGO_TARGET_DIR` must remain unset so each worktree builds into its own
`target/`.

### `.envrc` template (with [direnv](https://direnv.net/))

Drop this in the second worktree root:

```bash
export LAUNCHPAD_STUDIO_DATA_DIR="$HOME/.launchpad_studio-tools"
export AO_PORT=3101
export LAUNCHPAD_VITE_PORT=1430
export VITE_API_BASE_URL="http://localhost:3101"
unset CARGO_TARGET_DIR
```

### Running both apps

**Default checkout (default ports):**
```bash
cd frontend
npm run tauri dev
```

**Second checkout (alt ports):** a Tauri config overlay flips `devUrl` and the
dev-server port:
```bash
cd frontend
npm run tauri dev -- --config src-tauri/tauri.conf.tools.json
```

Both apps run concurrently, write to separate user data dirs, and bind separate
ports.

## Conventions for tool / feature implementations

- **Never hardcode `~/.launchpad_studio`.** Always resolve the data root through
  `ao_protocol::data_root::resolve_data_root()` (or `DataRoot::resolve()` for the
  typed wrapper).
- **Never hardcode `3001` / `1420`.** Read the env var, or accept a port from a
  higher-level config.
- **Don't write outside `LAUNCHPAD_STUDIO_DATA_DIR`** for per-user state.
  Project-local state (e.g. `effective_cwd/.launchpad_studio/`) is a separate
  concept; if you need that, document it explicitly.
