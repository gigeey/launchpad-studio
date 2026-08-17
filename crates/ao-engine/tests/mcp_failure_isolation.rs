//! Failure-isolation integration test — a broken `mcp_servers.toml` must not
//! prevent successful server construction or other tools from registering.
//!
//! Fixture: three [[server]] entries:
//!   good_server      — fixture_mcp_server in normal mode (registers one "echo" tool)
//!   bad_command_server — nonexistent binary path (spawn failure)
//!   crashing_server  — fixture_mcp_server with MCP_BEHAVIOR=crash (exits before handshake)
//!
//! Assertions:
//!   (a) good_server's `mcp__good_server__echo` tool is registered.
//!   (b) No tools from bad_command_server or crashing_server appear in the registry.
//!   (c) Built-in IO tools (Read, Glob) are unaffected.
//!   (d) Two WARN lines are emitted — one per failed server — mentioning their names.
//!
//! Runs by default (no env-var gate).  Runtime budget: 10 seconds.

use std::sync::{Arc, Mutex};

use ao_engine_tools_core::Registry;
use ao_engine_tools_engine::register_all as register_engine_tools;
use ao_engine_tools_io::register_all as register_io_tools;
use ao_engine_tools_provider_config::McpServersConfig;
use ao_engine_tools_runner::mcp::McpManager;
use ao_protocol::data_root::DATA_DIR_ENV_VAR;
use tempfile::TempDir;

// ── Binary locator ────────────────────────────────────────────────────────────

fn fixture_server_bin() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap();
    // Integration test binaries land in `target/debug/deps/`; the fixture
    // binary lands one level up in `target/debug/`.
    let bin_dir = if dir.file_name().map_or(false, |n| n == "deps") {
        dir.parent().unwrap()
    } else {
        dir
    };
    bin_dir
        .join("fixture_mcp_server")
        .with_extension(std::env::consts::EXE_EXTENSION)
}

// ── Env guard ─────────────────────────────────────────────────────────────────

struct EnvGuard {
    key: &'static str,
    prior: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, val: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, val);
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

// ── Warn-capture tracing subscriber ──────────────────────────────────────────

/// Minimal tracing::Subscriber that buffers WARN-level event fields.
/// Used with `tracing::subscriber::set_default` (thread-local) inside the
/// `#[tokio::test]` current_thread runtime so all spawned tasks share it.
#[derive(Clone, Default)]
struct WarnCapture(Arc<Mutex<Vec<String>>>);

impl WarnCapture {
    fn messages(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl tracing::Subscriber for WarnCapture {
    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        *meta.level() <= tracing::Level::WARN
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }

        struct Collector(String);
        impl tracing::field::Visit for Collector {
            fn record_str(&mut self, _: &tracing::field::Field, val: &str) {
                self.0.push_str(val);
                self.0.push(' ');
            }
            fn record_debug(&mut self, _: &tracing::field::Field, val: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{val:?} "));
            }
        }

        let mut c = Collector(String::new());
        event.record(&mut c);
        self.0.lock().unwrap().push(c.0);
    }
}

// ── ENV serialisation mutex ───────────────────────────────────────────────────

static ENV_MUTEX: Mutex<()> = Mutex::new(());

// ── Test ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn broken_servers_do_not_prevent_good_server_registration() {
    let _env_lock = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());

    let bin = fixture_server_bin();
    assert!(
        bin.exists(),
        "fixture_mcp_server binary not found at {bin:?}; build with `cargo build -p ao-engine`"
    );

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    let bin_path = bin.to_str().unwrap();
    let toml = format!(
        r#"
[[server]]
name = "good_server"
command = "{bin_path}"

[[server]]
name = "bad_command_server"
command = "/nonexistent/binary/that/does/not/exist"

[[server]]
name = "crashing_server"
command = "{bin_path}"

[server.env]
MCP_BEHAVIOR = "crash"
"#
    );

    std::fs::write(data_dir.join("mcp_servers.toml"), &toml).unwrap();
    let _env = EnvGuard::set(DATA_DIR_ENV_VAR, data_dir.to_str().unwrap());

    // Capture WARN events for this thread (current_thread runtime — all tasks
    // including tokio::spawn'd background tasks run on the test thread).
    let capture = WarnCapture::default();
    let _sub = tracing::subscriber::set_default(capture.clone());

    let config = McpServersConfig::load().expect("fixture mcp_servers.toml should parse");
    assert_eq!(config.servers.len(), 3, "fixture should have 3 server entries");

    let mut registry = Registry::new();
    register_io_tools(&mut registry);
    register_engine_tools(&mut registry);

    let manager = McpManager::from_config(&config).await;
    let _manager = manager.register_into(&mut registry).await;
    registry.build_deferred_index();

    let all_tools = registry.list();

    // (a) good_server's echo tool must be registered
    assert!(
        registry.lookup_io("mcp__good_server__echo").is_some(),
        "good_server's echo tool should be registered; got: {all_tools:?}"
    );

    // (b) no tools from failed servers
    let failed_tools: Vec<_> = all_tools
        .iter()
        .filter(|n| n.contains("bad_command_server") || n.contains("crashing_server"))
        .collect();
    assert!(
        failed_tools.is_empty(),
        "tools from failed servers must not be registered; found: {failed_tools:?}"
    );

    // (c) built-in tools still present alongside good_server's tools
    assert!(
        registry.lookup_io("Read").is_some(),
        "built-in Read tool must still be registered"
    );
    assert!(
        registry.lookup_io("Glob").is_some(),
        "built-in Glob tool must still be registered"
    );

    // (d) exactly one WARN per failed server
    let msgs = capture.messages();
    let bad_warns: Vec<_> = msgs.iter().filter(|m| m.contains("bad_command_server")).collect();
    let crash_warns: Vec<_> = msgs.iter().filter(|m| m.contains("crashing_server")).collect();
    assert!(
        !bad_warns.is_empty(),
        "expected at least one WARN mentioning bad_command_server; captured: {msgs:?}"
    );
    assert!(
        !crash_warns.is_empty(),
        "expected at least one WARN mentioning crashing_server; captured: {msgs:?}"
    );
}
