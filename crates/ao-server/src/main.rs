use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::EnvFilter;

/// Deliberately synchronous and deliberately not `#[tokio::main]`: the
/// keychain-forbidden propagation below must run before any additional
/// thread exists (see `propagate_keychain_forbidden`'s doc), and a
/// `#[tokio::main] async fn main()` fails that requirement — the runtime
/// the macro builds spawns its worker threads before the body of the
/// annotated `async fn` ever starts running, so even a call on its very
/// first line would already be too late. Building the runtime by hand
/// here, after the propagation call, keeps the ordering explicit.
fn main() {
    // Reify this process's keychain-forbidden determination into its own
    // environment before anything else runs, so every child this server
    // ever spawns for the rest of its life — a background agent's CLI
    // runner, a tool that shells out, anything not yet written — inherits
    // it automatically instead of each spawn site needing its own wiring.
    // Must stay the first statement in `main`: see
    // `propagate_keychain_forbidden`'s doc for why.
    ao_engine_tools_provider_config::propagate_keychain_forbidden();

    let runtime = tokio::runtime::Runtime::new().expect("failed to build the tokio runtime");
    runtime.block_on(run());
}

async fn run() {
    // Initialize tracing with env filter
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    EnvFilter::new(
                        "ao_server=debug,ao_engine=debug,ao_process=debug,ao_normalizer=trace,ao_persistence=debug",
                    )
                }),
        )
        .init();

    // The server is a headless daemon — it has no window to prompt through,
    // so any macOS keychain "wants to use your confidential information"
    // modal it might otherwise draw would just hang the request with nobody
    // present to answer it. Only the Tauri desktop app is a legitimate
    // surface for that one-time authorization prompt. That reasoning only
    // holds for a server nobody is sitting in front of, though — prompts
    // are suppressed only when `should_suppress_keychain_prompts` says to
    // (i.e. the keychain is already off-limits to this process), not
    // unconditionally, so an interactively-run server can still prompt its
    // developer. See that function's docs for the full precedence.
    if ao_engine_tools_provider_config::should_suppress_keychain_prompts() {
        ao_engine_tools_provider_config::disable_interactive_keychain_prompts();
    }

    // Surface a loud, one-time warning if the AgentWatch poll-interval floor
    // is running below its shipped default — this knob exists only for
    // demos/local testing (see `MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR`)
    // and silently lowering it multiplies background model-session cost for
    // every enabled AgentWatch.
    {
        let effective_min_poll =
            ao_protocol::assignment::effective_min_agent_watch_poll_interval_secs();
        let default_min_poll = ao_protocol::assignment::MIN_AGENT_WATCH_POLL_INTERVAL_SECS;
        if effective_min_poll < default_min_poll {
            tracing::warn!(
                effective_min_agent_watch_poll_interval_secs = effective_min_poll,
                default_min_agent_watch_poll_interval_secs = default_min_poll,
                "AgentWatch minimum poll interval is running at {}s via {} — this is a \
                 non-default, cost-increasing override for demos/testing only and must not \
                 be set in a shipped or production environment",
                effective_min_poll,
                ao_protocol::assignment::MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR,
            );
        }
    }

    // CLI subcommand dispatch
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("migrate-skills") {
        let data_dir = ao_protocol::data_root::resolve_data_root()
            .expect("Failed to resolve data root");
        match ao_server::migrate_skills::run(&data_dir).await {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                tracing::error!("migrate-skills failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Build AppState
    let state = Arc::new(
        ao_engine::AppState::new()
            .await
            .expect("Failed to initialize AppState"),
    );

    // Spawn background cleanup task: run on startup, then every 30 minutes
    {
        let persistence = Arc::clone(&state.persistence);
        tokio::spawn(async move {
            let cleanup_interval = Duration::from_secs(30 * 60);
            let older_than = Duration::from_secs(3600);

            loop {
                match persistence.assets.cleanup_all_uncommitted(older_than).await {
                    Ok(results) => {
                        for (agent_id, count, freed) in &results {
                            tracing::info!(
                                "Cleaned up {} orphaned files for agent {}, freed {} bytes",
                                count,
                                agent_id,
                                freed
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Asset cleanup failed: {}", e);
                    }
                }
                tokio::time::sleep(cleanup_interval).await;
            }
        });
    }

    // Spawn background MCP session TTL sweep: evicts orphaned entries from crashed
    // CLI processes that never sent DELETE /sessions. Runs every 10 minutes.
    // TTL is configurable via LAUNCHPAD_MCP_SESSION_TTL_SECS (default: 3600).
    {
        let mcp_sessions = Arc::clone(&state.mcp_sessions);
        let ttl_secs: u64 = std::env::var("LAUNCHPAD_MCP_SESSION_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);
        let ttl = Duration::from_secs(ttl_secs);

        // Surface (never silently clamp) the cross-config trap where the
        // sync-form deadline (`AO_SYNC_FORM_TIMEOUT_SECS`, resolved by
        // `ask_user_question_form::sync_form_timeout`) reaches or exceeds this
        // TTL: a long-outstanding form then gets cancelled by session expiry
        // before it can ever reach its own timeout branch. See
        // `ask_user_question_form::check_sync_form_timeout_vs_session_ttl`.
        let form_timeout = ao_engine_tools_engine::ask_user_question_form::sync_form_timeout();
        if let Some(warning) =
            ao_engine_tools_engine::ask_user_question_form::check_sync_form_timeout_vs_session_ttl(
                form_timeout,
                ttl,
            )
        {
            tracing::warn!("{}", warning);
        }

        tokio::spawn(async move {
            let sweep_interval = Duration::from_secs(10 * 60);
            loop {
                tokio::time::sleep(sweep_interval).await;
                let evicted = mcp_sessions.sweep_expired_sessions(ttl);
                if evicted > 0 {
                    tracing::info!("MCP session TTL sweep evicted {} orphaned session(s)", evicted);
                }
            }
        });
    }

    // Spawn background staged-memory TTL sweep: bounds the "Pending review"
    // backlog (`ReflectionStagingStore`) by expiring candidates nobody
    // reviewed within `STAGED_CANDIDATE_TTL_DAYS` (default 7 — tune that
    // constant in `ao-engine-tools-engine/src/memory/staged_ttl.rs`).
    // Runs on startup (so an existing backlog drains on the very first
    // sweep, not just candidates staged from here on) and then on the same
    // periodic-maintenance cadence as the asset-cleanup/MCP-session sweeps
    // above — there is no other existing scheduler for this store to
    // co-locate with.
    {
        let persistence = Arc::clone(&state.persistence);
        tokio::spawn(async move {
            let sweep_interval = Duration::from_secs(30 * 60);
            let ttl = chrono::Duration::days(
                ao_engine_tools_engine::memory::STAGED_CANDIDATE_TTL_DAYS,
            );

            loop {
                match persistence.agents.list().await {
                    Ok(agents) => {
                        let now = chrono::Utc::now();
                        let mut total_expired = 0usize;
                        for agent in &agents {
                            match ao_engine_tools_engine::memory::sweep_expired_staged_candidates(
                                &persistence.reflection_staging,
                                &agent.id,
                                now,
                                ttl,
                            )
                            .await
                            {
                                Ok(count) => total_expired += count,
                                Err(e) => tracing::warn!(
                                    agent_id = %agent.id,
                                    "staged-memory TTL sweep failed: {}",
                                    e
                                ),
                            }
                        }
                        if total_expired > 0 {
                            tracing::info!(
                                "staged-memory TTL sweep expired {} candidate(s) across {} agent(s)",
                                total_expired,
                                agents.len()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("staged-memory TTL sweep: failed to list agents: {}", e);
                    }
                }
                tokio::time::sleep(sweep_interval).await;
            }
        });
    }

    // Read port from env
    let port: u16 = std::env::var("AO_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);

    // Best-effort in-use lock: records this process's pid (and listening
    // port) in the data root it's actually running against, so a later
    // `POST /workspaces/{id}/activate` call targeting this same root from a
    // DIFFERENT process refuses instead of racing it. See
    // `ao_server::workspace_lock` for the full contract — failures here are
    // logged, never fatal, and the lock only guards processes started after
    // it's written (an already-running process picks it up only on its next
    // restart). Resolved once and kept for the shutdown-side release below.
    let active_data_root = match ao_protocol::data_root::resolve_data_root() {
        Ok(root) => Some(root),
        Err(e) => {
            tracing::warn!("could not resolve data root for workspace lock: {e}");
            None
        }
    };
    if let Some(root) = &active_data_root {
        ao_server::workspace_lock::acquire_startup_lock(root, Some(port)).await;
    }

    // Bind host defaults to loopback — see `ao_server::webhook_gateway` for
    // why this must match what the webhook gateway's `INSECURE_NO_AUTH`
    // check reads: that check re-resolves this same env var per request
    // rather than trusting a value cached at startup.
    let bind_host = std::env::var(ao_server::webhook_gateway::BIND_HOST_ENV_VAR)
        .unwrap_or_else(|_| ao_server::webhook_gateway::DEFAULT_BIND_HOST.to_string());

    // Best-effort visibility sweep over existing webhook routes (see
    // `ao_server::routes::webhooks::validate_routes_at_startup`) — logs
    // misconfigured routes but never blocks boot.
    ao_server::routes::webhooks::validate_routes_at_startup(&state).await;

    let router = ao_server::routes::build_router(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", bind_host, port))
        .await
        .expect("Failed to bind to address");

    tracing::info!("ao-server listening on {}:{}", bind_host, port);
    tokio::select! {
        result = axum::serve(listener, router) => {
            result.expect("Server error");
        }
        _ = wait_for_termination_signal() => {
            tracing::info!("ao-server: termination signal received, shutting down");
            shut_down_channel_bridge(&state).await;
            if let Some(root) = &active_data_root {
                ao_server::workspace_lock::release_lock(root).await;
            }
        }
    }
}

/// Resolves once this process receives a termination signal: SIGTERM (the
/// one a process manager / `docker stop` / rolling-deploy replacement
/// sends) or Ctrl-C, whichever comes first. `ctrl_c()` alone (Rust's usual
/// go-to) only ever covers the latter — SIGTERM has no built-in handler at
/// all, so without this a `kill <pid>` hits the OS's default disposition
/// (immediate termination) and no cleanup code, including the channel
/// lease release below, ever runs.
#[cfg(unix)]
async fn wait_for_termination_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install a SIGTERM handler");
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_termination_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Releases every single-writer channel-binding lease
/// this process holds, and *waits* for that release to actually finish
/// before returning — so a standby process on the same data root can
/// reclaim a binding immediately on a graceful stop, instead of waiting out
/// the full lease TTL as it would on a hard kill. Sending on
/// `telegram_bridge_shutdown` only requests the stop; the release itself is
/// async work the reconcile-loop task does afterward, which is exactly what
/// awaiting `telegram_bridge_join_handle` confirms completed. Bounded by a
/// generous timeout as a safety net, not a routine wait — every release is
/// one small file write per held binding, so this should resolve in well
/// under a second in practice.
async fn shut_down_channel_bridge(state: &ao_engine::AppState) {
    let _ = state.telegram_bridge_shutdown.send(());
    let handle = state.telegram_bridge_join_handle.lock().await.take();
    let Some(handle) = handle else { return };
    match tokio::time::timeout(Duration::from_secs(10), handle).await {
        Ok(Ok(())) => tracing::info!("ao-server: channel bridge released its leases and stopped"),
        Ok(Err(e)) => tracing::warn!("ao-server: channel bridge shutdown task panicked: {e}"),
        Err(_) => tracing::warn!(
            "ao-server: channel bridge did not finish releasing its leases within the shutdown grace period"
        ),
    }
}
