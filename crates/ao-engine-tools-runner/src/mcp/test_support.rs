//! Shared test-only helpers for the MCP unit tests.
//!
//! Compiled only under `cfg(test)`; nothing here ships in a release build.

/// Locate the `echo_mcp_server` fixture binary, panicking with the remedy if
/// it is missing.
///
/// # Why this is a disk search and not `CARGO_BIN_EXE_echo_mcp_server`
///
/// Cargo sets `CARGO_BIN_EXE_<name>` only when compiling *integration* tests
/// (those under `tests/`). The MCP tests that need this fixture are unit tests
/// living in `src/`, because they assert against module-private items —
/// `McpManager::clients` and `McpManager::resolve_reauth_target`, among others
/// — that an integration test cannot reach. So they locate the binary by
/// walking up from the current test executable instead.
///
/// # The caveat that costs people time
///
/// Because the path is resolved at runtime off the filesystem, **cargo has no
/// dependency edge from these tests to the fixture binary**. Two consequences,
/// both real:
///
/// 1. `cargo test --lib` does not *build* the fixture. On a fresh checkout the
///    first `--lib` run fails ~51 tests that have nothing to do with your
///    change. Running plain `cargo test` once fixes it permanently for that
///    target directory.
/// 2. `cargo test --lib` does not *rebuild* a stale fixture either. After
///    editing `tests/fixtures/echo_mcp_server.rs`, a `--lib` run silently
///    tests against the previous build and passes. Re-run without `--lib`.
///
/// The panic below covers case 1 loudly. Case 2 cannot be detected from here —
/// a stale binary is indistinguishable from a current one — which is why it is
/// documented rather than checked.
pub(crate) fn echo_server_bin() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe() should resolve for a test binary");
    let dir = exe.parent().expect("test binary should have a parent dir");
    // Unit-test binaries land in `target/<profile>/deps/`; the fixture `[[bin]]`
    // lands one level up in `target/<profile>/`.
    let bin_dir = if dir.file_name().is_some_and(|n| n == "deps") {
        dir.parent().expect("deps/ should have a parent dir")
    } else {
        dir
    };
    let path = bin_dir
        .join("echo_mcp_server")
        .with_extension(std::env::consts::EXE_EXTENSION);

    assert!(
        path.exists(),
        "echo_mcp_server fixture binary not found at {}\n\
         \n\
         This test spawns the `echo_mcp_server` fixture, but cargo has no\n\
         dependency edge to it (see mcp::test_support for why), so `--lib`\n\
         never builds it.\n\
         \n\
         Fix: run `cargo test -p ao-engine-tools-runner` once (no `--lib`),\n\
         or `cargo build -p ao-engine-tools-runner --bins` first.\n\
         \n\
         Note: `--lib` also will not REBUILD a stale fixture. After editing\n\
         tests/fixtures/echo_mcp_server.rs, re-run without `--lib`.",
        path.display()
    );

    path
}
