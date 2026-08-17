/// Subscribe to `bus` and forward every broadcast event into an mpsc so
/// tests can use `recv()` / `try_recv()` without the lag-tracking overhead
/// of a raw `broadcast::Receiver`.
#[cfg(test)]
pub(crate) fn capture(
    bus: &std::sync::Arc<crate::event_bus::EventBus>,
) -> tokio::sync::mpsc::UnboundedReceiver<ao_protocol::event::AgentEvent> {
    let mut bcast = bus.subscribe();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match bcast.recv().await {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use crate::agent_runner::{AgentRunner, AgentRunnerMode, CliAgentRunner};

    #[test]
    fn trait_object_compiles_for_cli_runner() {
        // Full construction requires process supervisor — just verify the type compiles.
        let _ = |runner: CliAgentRunner| {
            let arc: Arc<dyn AgentRunner> = Arc::new(runner);
            assert_eq!(arc.mode(), AgentRunnerMode::Cli);
        };
    }

    /// Verify that `AppState::new` populates the tools registry with the full
    /// IO + engine + Delegate/DelegateOutput/DelegateStop catalog.
    ///
    /// Uses the crate-wide `LAUNCHPAD_STUDIO_DATA_DIR` env-var lock shared with
    /// `plugin_paths` and `plugin_registry` tests so env mutations do not race.
    #[tokio::test]
    async fn app_state_new_populates_tools_registry() {
        let tmp = tempfile::tempdir().expect("temp dir");
        // Serialize against all tests in this crate that mutate
        // LAUNCHPAD_STUDIO_DATA_DIR (plugin_paths, plugin_registry, etc.).
        let _guard = crate::plugin_paths::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());

        let state = crate::state::AppState::new()
            .await
            .expect("AppState::new must succeed with a temp data dir");

        let names = state.tools_registry.list();
        assert!(
            state.tools_registry.len() >= 20,
            "expected >= 20 tools (IO + engine + Delegate suite), got {} — {names:?}",
            state.tools_registry.len(),
        );
        for required in ["Read", "Grep", "Bash", "TodoWrite", "Delegate", "DelegateOutput", "DelegateStop"] {
            assert!(
                names.iter().any(|n| n == required),
                "expected tool '{required}' in registry, got: {names:?}",
            );
        }

        std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
    }
}
